use itertools::Itertools;
use miette::{Context, Diagnostic, IntoDiagnostic, NamedSource, SourceSpan};
use pep508_rs::Requirement;
use pixi_config::Config;
use rattler_conda_types::{MatchSpec, NamedChannelOrUrl, ParseStrictness::Lenient};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::{io::BufRead, path::Path, str::FromStr};
use thiserror::Error;
use uv_requirements_txt::{RequirementsTxt, RequirementsTxtRequirement};

#[derive(Debug, Error)]
#[error("Failed to parse '{path}' as a conda environment file")]
struct YamlParseError {
    #[source]
    source: serde_yaml::Error,
    src: NamedSource<String>,
    span: Option<SourceSpan>,
    path: PathBuf,
}

impl Diagnostic for YamlParseError {
    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        Some(&self.src)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
        self.span.as_ref().map(|span| {
            Box::new(std::iter::once(miette::LabeledSpan::new(
                Some("error occurred here".to_string()),
                span.offset(),
                span.len(),
            ))) as Box<dyn Iterator<Item = miette::LabeledSpan>>
        })
    }
}

impl YamlParseError {
    fn new(src: NamedSource<String>, source: serde_yaml::Error, path: PathBuf) -> Self {
        let span = source.location().map(|loc| {
            let start = loc.index();
            let end = start + 1; // Could expand this to a larger span if needed
            (start..end).into()
        });
        Self {
            src,
            source,
            span,
            path,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct CondaEnvFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    channels: Vec<NamedChannelOrUrl>,
    dependencies: Vec<CondaEnvDep>,
    #[serde(default)]
    variables: HashMap<String, String>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum CondaEnvDep {
    Conda(String),
    Pip {
        #[serde(default)]
        pip: Option<Vec<String>>,
    },
}

type ParsedDependencies = (
    Vec<MatchSpec>,
    Vec<pep508_rs::Requirement>,
    Vec<NamedChannelOrUrl>,
);

impl CondaEnvFile {
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn channels(&self) -> &Vec<NamedChannelOrUrl> {
        &self.channels
    }

    fn dependencies(&self) -> &Vec<CondaEnvDep> {
        &self.dependencies
    }

    pub fn variables(&self) -> HashMap<String, String> {
        self.variables.clone()
    }

    pub fn from_path(path: &Path) -> miette::Result<Self> {
        let file = fs_err::File::open(path).into_diagnostic()?;
        let reader = std::io::BufReader::new(file);

        let lines = reader
            .lines()
            .collect::<Result<Vec<String>, _>>()
            .into_diagnostic()?;
        let mut s = String::new();
        for line in lines {
            if line.contains("- sel(") {
                tracing::warn!("Skipping micromamba sel(...) in line: \"{}\"", line.trim());
                tracing::warn!("Please add the dependencies manually");
                continue;
            }
            s.push_str(&line);
            s.push('\n');
        }
        let mut env_file: CondaEnvFile = match serde_yaml::from_str(&s) {
            Ok(env_file) => env_file,
            Err(e) => {
                let src = NamedSource::new(path.display().to_string(), s.to_string());
                let error = YamlParseError::new(src, e, path.to_path_buf());
                return Err(miette::Report::new(error));
            }
        };
        env_file.path = path.to_path_buf();
        Ok(env_file)
    }

    pub async fn to_manifest(
        self: CondaEnvFile,
        config: &Config,
    ) -> miette::Result<(
        Vec<MatchSpec>,
        Vec<pep508_rs::Requirement>,
        Vec<NamedChannelOrUrl>,
    )> {
        // TODO: should we be applying `config.channel_config` for parsed channels too?
        let mut channels = parse_channels(self.channels().clone());

        let parent_dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let (conda_deps, pip_deps, extra_channels) =
            parse_dependencies(self.dependencies().clone(), parent_dir).await?;

        channels.extend(extra_channels);
        let mut channels: Vec<_> = channels.into_iter().unique().collect();
        if channels.is_empty() {
            channels = config.default_channels();
        }

        Ok((conda_deps, pip_deps, channels))
    }
}

async fn parse_dependencies(
    deps: Vec<CondaEnvDep>,
    base_dir: &Path,
) -> miette::Result<ParsedDependencies> {
    let mut conda_deps = Vec::new();
    let mut pip_deps = Vec::new();
    let mut picked_up_channels = Vec::new();
    for dep in deps {
        match dep {
            CondaEnvDep::Conda(d) => {
                parse_conde_dep(d, &mut conda_deps, &mut picked_up_channels)?;
            }
            CondaEnvDep::Pip { pip } => {
                parse_pip_dep(pip, base_dir, &mut pip_deps).await?;
            }
        }
    }

    Ok((conda_deps, pip_deps, picked_up_channels))
}

fn parse_conde_dep(
    dep: String,
    conda_deps: &mut Vec<MatchSpec>,
    picked_up_channels: &mut Vec<NamedChannelOrUrl>,
) -> miette::Result<()> {
    let match_spec = MatchSpec::from_str(&dep, Lenient)
        .into_diagnostic()
        .wrap_err(format!("Can't parse '{dep}' as conda dependency"))?;
    if let Some(channel) = &match_spec.channel {
        picked_up_channels.push(
            // named channels are given a url with default channel config in `MatchSpec::from_str`
            NamedChannelOrUrl::from_str(channel.base_url.as_str())
                .into_diagnostic()
                .wrap_err(format!("can't parse '{}' as channel", channel.base_url))?,
        );
    }
    conda_deps.push(match_spec);
    Ok(())
}

async fn parse_pip_dep(
    pip: Option<Vec<String>>,
    base_dir: &Path,
    pip_deps: &mut Vec<Requirement>,
) -> miette::Result<()> {
    let pip = pip.unwrap_or_default();
    for dep in pip {
        if let Some(filename) = extract_requirements_filename(&dep) {
            let full_path = base_dir.join(&filename);
            let requirements_file = RequirementsTxt::parse(&full_path, &base_dir)
                .await
                .into_diagnostic()?;
            for entry in requirements_file.requirements {
                match entry.requirement {
                    RequirementsTxtRequirement::Named(uv_req) => {
                        let req_str = uv_req.to_string();

                        let core_requirement = pep508_rs::Requirement::from_str(&req_str)
                            .into_diagnostic()
                            .wrap_err_with(|| {
                                format!("Failed to convert requirements.txt dependency {}", req_str)
                            })?;

                        pip_deps.push(core_requirement);
                    }

                    RequirementsTxtRequirement::Unnamed(_unnamed_req) => {
                        return Err(miette::miette!(
                            "Unnamed direct URL dependencies in requirements.txt are not currently supported by Pixi"
                        ));
                    }
                }
            }
        } else {
            let requirement = pep508_rs::Requirement::from_str(&dep)
                .into_diagnostic()
                .wrap_err(format!("Can't parse '{dep}' as pypi dependency"))?;
            pip_deps.push(requirement);
        }
    }
    Ok(())
}

fn extract_requirements_filename(dep: &str) -> Option<String> {
    for prefix in ["--requirement ", "--requirement=", "-r ", "-r"] {
        if let Some(rest) = dep.strip_prefix(prefix) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn parse_channels(channels: Vec<NamedChannelOrUrl>) -> Vec<NamedChannelOrUrl> {
    let mut new_channels = vec![];
    for channel in channels {
        if channel.as_str() == "defaults" {
            // https://docs.anaconda.com/free/working-with-conda/reference/default-repositories/#active-default-channels
            new_channels.push(NamedChannelOrUrl::Name("main".to_string()));
            new_channels.push(NamedChannelOrUrl::Name("r".to_string()));
            new_channels.push(NamedChannelOrUrl::Name("msys2".to_string()));
        } else {
            new_channels.push(channel);
        }
    }
    new_channels
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::Path, str::FromStr};

    use rattler_conda_types::{Channel, ChannelConfig, MatchSpec, ParseStrictness::Strict};

    use super::*;

    #[tokio::test]
    async fn test_parse_conda_env_file() {
        let example_conda_env_file = r#"
        name: pixi_example_project
        channels:
          - conda-forge
          - https://custom-server.com/channel
        dependencies:
          - python
          - pytorch::torchvision
          - conda-forge::pytest
          - wheel=0.31.1
          - sel(linux): blabla
          - foo >=1.2.3.*  # only valid when parsing in lenient mode
          - pip:
            - requests
            - deepobs @ git+https://git@github.com/fsschneider/DeepOBS.git@develop
            - torch==1.8.1
        "#;

        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(example_conda_env_file.as_bytes()).unwrap();
        let (_file, path) = f.into_parts();

        let conda_env_file_data = CondaEnvFile::from_path(&path).unwrap();
        let channel_config = ChannelConfig::default_with_root_dir(
            std::env::current_dir().expect("Could not get current directory"),
        );

        assert_eq!(conda_env_file_data.name(), Some("pixi_example_project"));
        assert_eq!(
            conda_env_file_data.channels(),
            &vec![
                NamedChannelOrUrl::from_str("conda-forge").unwrap(),
                NamedChannelOrUrl::from_str("https://custom-server.com/channel").unwrap(),
            ]
        );

        let config = Config::default();
        let (conda_deps, pip_deps, channels) =
            conda_env_file_data.to_manifest(&config).await.unwrap();

        assert_eq!(
            channels,
            vec![
                NamedChannelOrUrl::from_str("conda-forge").unwrap(),
                NamedChannelOrUrl::from_str("https://custom-server.com/channel").unwrap(),
                NamedChannelOrUrl::from_str(
                    Channel::from_str("pytorch", &channel_config)
                        .unwrap()
                        .base_url
                        .as_str()
                )
                .unwrap(),
                NamedChannelOrUrl::from_str(
                    Channel::from_str("conda-forge", &channel_config)
                        .unwrap()
                        .base_url
                        .as_str()
                )
                .unwrap(),
            ]
        );

        println!("{conda_deps:?}");
        assert_eq!(
            conda_deps,
            vec![
                MatchSpec::from_str("python", Strict).unwrap(),
                MatchSpec::from_str("pytorch::torchvision", Strict).unwrap(),
                MatchSpec::from_str("conda-forge::pytest", Strict).unwrap(),
                MatchSpec::from_str("wheel=0.31.1", Strict).unwrap(),
                MatchSpec::from_str("foo >=1.2.3", Strict).unwrap(),
            ]
        );

        assert_eq!(
            pip_deps,
            vec![
                pep508_rs::Requirement::from_str("requests").unwrap(),
                pep508_rs::Requirement::from_str(
                    "deepobs @ git+https://git@github.com/fsschneider/DeepOBS.git@develop"
                )
                .unwrap(),
                pep508_rs::Requirement::from_str("torch==1.8.1").unwrap(),
            ]
        );
    }

    #[tokio::test]
    async fn test_import_from_env_yamls() {
        let test_files_path = Path::new(&env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("environment_yamls");

        let entries = match fs_err::read_dir(test_files_path.clone()) {
            Ok(entries) => entries,
            Err(e) => panic!("Failed to read directory: {e}"),
        };

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.expect("Failed to read directory entry");
            if entry.path().is_file() {
                paths.push(entry.path());
            }
        }

        for path in paths {
            let env_info = CondaEnvFile::from_path(&path).unwrap();
            // Try `cargo insta test` to run all at once
            let snapshot_name = format!(
                "test_import_from_env_yaml.{}",
                path.file_name().unwrap().to_string_lossy()
            );

            insta::assert_debug_snapshot!(
                snapshot_name,
                (
                    parse_dependencies(env_info.dependencies().clone(), &test_files_path)
                        .await
                        .unwrap(),
                    parse_channels(env_info.channels().clone()),
                    env_info.name()
                )
            );
        }
    }

    #[tokio::test]
    async fn test_parse_conda_env_file_with_explicit_pip_dep() {
        let example_conda_env_file = r#"
        name: pixi_example_project
        channels:
          - conda-forge
        dependencies:
          - pip==24.0
          - pip:
            - requests
        "#;

        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path();
        let mut file = fs_err::File::create(path).unwrap();
        file.write_all(example_conda_env_file.as_bytes()).unwrap();

        let conda_env_file_data = CondaEnvFile::from_path(path).unwrap();
        let vars = conda_env_file_data.variables();
        let base_dir = &conda_env_file_data.path;
        let (conda_deps, pip_deps, _) =
            parse_dependencies(conda_env_file_data.dependencies().clone(), base_dir)
                .await
                .unwrap();

        assert_eq!(
            conda_deps,
            vec![MatchSpec::from_str("pip==24.0", Strict).unwrap(),]
        );

        assert_eq!(
            pip_deps,
            vec![pep508_rs::Requirement::from_str("requests").unwrap()]
        );

        let empty_map = HashMap::<String, String>::new();

        assert_eq!(vars, empty_map);
    }

    #[tokio::test]
    async fn test_parse_conda_env_file_with_pip_requirements_file() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let base_dir = tmp_dir.path().to_path_buf();

        let requirements_file_name = "requirements.txt";
        let example_conda_env_file = format!(
            r#"
        name: pixi_example_project
        channels:
          - conda-forge
        dependencies:
          - pip==24.0
          - pip:
            - -r {file_name}
        "#,
            file_name = requirements_file_name
        );

        let example_requirements = r#"
        requests==2.31.0
        numpy>=1.26.0
        pandas~=2.2.0
        scipy
        "#;

        let conda_env_path = base_dir.join("environment.yml");
        fs_err::write(&conda_env_path, example_conda_env_file).unwrap();

        let requirements_path = base_dir.join(requirements_file_name);
        fs_err::write(&requirements_path, example_requirements).unwrap();

        let conda_env_file_data = CondaEnvFile::from_path(&conda_env_path).unwrap();
        let vars = conda_env_file_data.variables();
        let (conda_deps, pip_deps, _) =
            parse_dependencies(conda_env_file_data.dependencies().clone(), &base_dir)
                .await
                .unwrap();

        assert_eq!(
            conda_deps,
            vec![MatchSpec::from_str("pip==24.0", Strict).unwrap(),]
        );

        assert_eq!(
            pip_deps,
            vec![
                pep508_rs::Requirement::from_str("requests==2.31.0").unwrap(),
                pep508_rs::Requirement::from_str("numpy>=1.26.0").unwrap(),
                pep508_rs::Requirement::from_str("pandas~=2.2.0").unwrap(),
                pep508_rs::Requirement::from_str("scipy").unwrap(),
            ]
        );

        let empty_map = HashMap::<String, String>::new();

        assert_eq!(vars, empty_map);
    }

    #[test]
    fn test_parse_conda_env_file_with_variables() {
        let example_conda_env_file = r#"
        name: pixi_example_project
        channels:
          - conda-forge
        dependencies:
          - pip==24.0
        variables:
          MY_VAR: my_value
          MY_OTHER_VAR: 123
          MY_EMPTY_VAR:
        "#;

        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path();
        let mut file = fs_err::File::create(path).unwrap();
        file.write_all(example_conda_env_file.as_bytes()).unwrap();

        let conda_env_file_data = CondaEnvFile::from_path(path).unwrap();
        let vars = conda_env_file_data.variables();

        assert_eq!(
            vars,
            HashMap::from([
                ("MY_VAR".to_string(), "my_value".to_string()),
                ("MY_OTHER_VAR".to_string(), "123".to_string()),
                ("MY_EMPTY_VAR".to_string(), "".to_string())
            ])
        );
    }
}
