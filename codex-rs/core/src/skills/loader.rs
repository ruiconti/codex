use crate::config::Config;
use crate::config_loader::ConfigLayerStack;
use crate::skills::model::SkillError;
use crate::skills::model::SkillLoadOutcome;
use crate::skills::model::SkillMetadata;
use crate::skills::system::system_cache_root_dir;
use codex_app_server_protocol::ConfigLayerSource;
use codex_protocol::protocol::SkillScope;
use dunce::canonicalize as normalize_path;
use serde::Deserialize;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use tracing::error;

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    metadata: SkillFrontmatterMetadata,
}

#[derive(Debug, Default, Deserialize)]
struct SkillFrontmatterMetadata {
    #[serde(default, rename = "short-description")]
    short_description: Option<String>,
}

const SKILLS_FILENAME: &str = "SKILL.md";
const SKILLS_DIR_NAME: &str = "skills";
const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 1024;
const MAX_SHORT_DESCRIPTION_LEN: usize = MAX_DESCRIPTION_LEN;

#[derive(Debug)]
enum SkillParseError {
    Read(std::io::Error),
    MissingFrontmatter,
    InvalidYaml(serde_yaml::Error),
    MissingField(&'static str),
    InvalidField { field: &'static str, reason: String },
}

impl fmt::Display for SkillParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillParseError::Read(e) => write!(f, "failed to read file: {e}"),
            SkillParseError::MissingFrontmatter => {
                write!(f, "missing YAML frontmatter delimited by ---")
            }
            SkillParseError::InvalidYaml(e) => write!(f, "invalid YAML: {e}"),
            SkillParseError::MissingField(field) => write!(f, "missing field `{field}`"),
            SkillParseError::InvalidField { field, reason } => {
                write!(f, "invalid {field}: {reason}")
            }
        }
    }
}

impl Error for SkillParseError {}

pub fn load_skills(config: &Config) -> SkillLoadOutcome {
    load_skills_from_roots(skill_roots(config))
}

pub(crate) struct SkillRoot {
    pub(crate) path: PathBuf,
    pub(crate) scope: SkillScope,
}

pub(crate) fn load_skills_from_roots<I>(roots: I) -> SkillLoadOutcome
where
    I: IntoIterator<Item = SkillRoot>,
{
    let mut outcome = SkillLoadOutcome::default();
    for root in roots {
        discover_skills_under_root(&root.path, root.scope, &mut outcome);
    }

    let mut seen: HashSet<String> = HashSet::new();
    outcome
        .skills
        .retain(|skill| seen.insert(skill.name.clone()));

    fn scope_rank(scope: SkillScope) -> u8 {
        // Higher-priority scopes first (matches dedupe priority order).
        match scope {
            SkillScope::Repo => 0,
            SkillScope::User => 1,
            SkillScope::System => 2,
            SkillScope::Admin => 3,
        }
    }

    outcome.skills.sort_by(|a, b| {
        scope_rank(a.scope)
            .cmp(&scope_rank(b.scope))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    });

    outcome
}

fn skill_roots_from_layer_stack_inner(config_layer_stack: &ConfigLayerStack) -> Vec<SkillRoot> {
    let mut roots = Vec::new();

    for layer in config_layer_stack.layers_high_to_low() {
        let Some(config_folder) = layer.config_folder() else {
            continue;
        };

        match &layer.name {
            ConfigLayerSource::Project { .. } => {
                roots.push(SkillRoot {
                    path: config_folder.as_path().join(SKILLS_DIR_NAME),
                    scope: SkillScope::Repo,
                });
            }
            ConfigLayerSource::User { .. } => {
                // `$CODEX_HOME/skills` (user-installed skills).
                roots.push(SkillRoot {
                    path: config_folder.as_path().join(SKILLS_DIR_NAME),
                    scope: SkillScope::User,
                });

                // Embedded system skills are cached under `$CODEX_HOME/skills/.system` and are a
                // special case (not a config layer).
                roots.push(SkillRoot {
                    path: system_cache_root_dir(config_folder.as_path()),
                    scope: SkillScope::System,
                });
            }
            ConfigLayerSource::System { .. } => {
                // The system config layer lives under `/etc/codex/` on Unix, so treat
                // `/etc/codex/skills` as admin-scoped skills.
                roots.push(SkillRoot {
                    path: config_folder.as_path().join(SKILLS_DIR_NAME),
                    scope: SkillScope::Admin,
                });
            }
            ConfigLayerSource::Mdm { .. }
            | ConfigLayerSource::SessionFlags
            | ConfigLayerSource::LegacyManagedConfigTomlFromFile { .. }
            | ConfigLayerSource::LegacyManagedConfigTomlFromMdm => {}
        }
    }

    roots
}

fn skill_roots(config: &Config) -> Vec<SkillRoot> {
    skill_roots_from_layer_stack_inner(&config.config_layer_stack)
}

pub(crate) fn skill_roots_from_layer_stack(
    config_layer_stack: &ConfigLayerStack,
) -> Vec<SkillRoot> {
    skill_roots_from_layer_stack_inner(config_layer_stack)
}

fn discover_skills_under_root(root: &Path, scope: SkillScope, outcome: &mut SkillLoadOutcome) {
    let Ok(root) = normalize_path(root) else {
        return;
    };

    if !root.is_dir() {
        return;
    }

    let mut queue: VecDeque<PathBuf> = VecDeque::from([root]);
    while let Some(dir) = queue.pop_front() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                error!("failed to read skills dir {}: {e:#}", dir.display());
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = match path.file_name().and_then(|f| f.to_str()) {
                Some(name) => name,
                None => continue,
            };

            if file_name.starts_with('.') {
                continue;
            }

            let Ok(file_type) = entry.file_type() else {
                continue;
            };

            if file_type.is_symlink() {
                continue;
            }

            if file_type.is_dir() {
                queue.push_back(path);
                continue;
            }

            if file_type.is_file() && file_name == SKILLS_FILENAME {
                match parse_skill_file(&path, scope) {
                    Ok(skill) => {
                        outcome.skills.push(skill);
                    }
                    Err(err) => {
                        if scope != SkillScope::System {
                            outcome.errors.push(SkillError {
                                path,
                                message: err.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }
}

fn parse_skill_file(path: &Path, scope: SkillScope) -> Result<SkillMetadata, SkillParseError> {
    let contents = fs::read_to_string(path).map_err(SkillParseError::Read)?;

    let frontmatter = extract_frontmatter(&contents).ok_or(SkillParseError::MissingFrontmatter)?;

    // Normalize the frontmatter to handle unquoted special characters
    let normalized_frontmatter = normalize_frontmatter_yaml(&frontmatter);

    let parsed: SkillFrontmatter =
        serde_yaml::from_str(&normalized_frontmatter).map_err(SkillParseError::InvalidYaml)?;

    let name = sanitize_single_line(&parsed.name);
    let description = sanitize_single_line(&parsed.description);
    let short_description = parsed
        .metadata
        .short_description
        .as_deref()
        .map(sanitize_single_line)
        .filter(|value| !value.is_empty());

    validate_field(&name, MAX_NAME_LEN, "name")?;
    validate_field(&description, MAX_DESCRIPTION_LEN, "description")?;
    if let Some(short_description) = short_description.as_deref() {
        validate_field(
            short_description,
            MAX_SHORT_DESCRIPTION_LEN,
            "metadata.short-description",
        )?;
    }

    let resolved_path = normalize_path(path).unwrap_or_else(|_| path.to_path_buf());

    Ok(SkillMetadata {
        name,
        description,
        short_description,
        path: resolved_path,
        scope,
    })
}

fn sanitize_single_line(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn validate_field(
    value: &str,
    max_len: usize,
    field_name: &'static str,
) -> Result<(), SkillParseError> {
    if value.is_empty() {
        return Err(SkillParseError::MissingField(field_name));
    }
    if value.chars().count() > max_len {
        return Err(SkillParseError::InvalidField {
            field: field_name,
            reason: format!("exceeds maximum length of {max_len} characters"),
        });
    }
    Ok(())
}

fn extract_frontmatter(contents: &str) -> Option<String> {
    let mut lines = contents.lines();
    if !matches!(lines.next(), Some(line) if line.trim() == "---") {
        return None;
    }

    let mut frontmatter_lines: Vec<&str> = Vec::new();
    let mut found_closing = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            found_closing = true;
            break;
        }
        frontmatter_lines.push(line);
    }

    if frontmatter_lines.is_empty() || !found_closing {
        return None;
    }

    Some(frontmatter_lines.join("\n"))
}

/// Normalizes YAML frontmatter by automatically quoting field values that contain
/// special YAML characters (like colons) which would otherwise cause parsing errors.
///
/// This function makes SKILL.md files more user-friendly by handling common cases
/// where users write descriptions like: "use when prompt: `pattern`" without quotes.
fn normalize_frontmatter_yaml(yaml: &str) -> String {
    let mut normalized_lines = Vec::new();

    for line in yaml.lines() {
        // Skip empty lines and lines that are already using block scalar syntax
        if line.trim().is_empty() || line.trim_start().starts_with("- ") {
            normalized_lines.push(line.to_string());
            continue;
        }

        // Check if this is a simple key-value line (not nested structures)
        if let Some(colon_pos) = line.find(':') {
            let before_colon = &line[..colon_pos];
            let after_colon = &line[colon_pos + 1..];

            // Only process top-level keys (name, description)
            // Skip if it's already using block scalar (|-) or if the value is already quoted
            let trimmed_after = after_colon.trim_start();
            if trimmed_after.starts_with("|-")
                || trimmed_after.starts_with("|+")
                || trimmed_after.starts_with('|')
                || trimmed_after.starts_with('"')
                || trimmed_after.starts_with('\'')
                || before_colon.trim_start() != before_colon.trim_start().trim()
            {
                normalized_lines.push(line.to_string());
                continue;
            }

            let value = after_colon.trim();

            // Check if value contains YAML special characters that need quoting
            // Focus on the colon which is the most common issue
            if !value.is_empty() && needs_quoting(value) {
                // Escape backslashes first, then double quotes
                // In YAML double-quoted strings, backslash is an escape character
                let escaped_value = value.replace('\\', "\\\\").replace('"', "\\\"");
                normalized_lines.push(format!("{}: \"{}\"", before_colon, escaped_value));
            } else {
                normalized_lines.push(line.to_string());
            }
        } else {
            normalized_lines.push(line.to_string());
        }
    }

    normalized_lines.join("\n")
}

/// Checks if a YAML value needs to be quoted to avoid parsing errors.
/// Returns true if the value contains special YAML characters like colons.
fn needs_quoting(value: &str) -> bool {
    // Check for colon followed by space (the most common problematic pattern)
    // e.g., "use when prompt: `pattern`" where ": `" triggers the error
    if value.contains(": ") {
        return true;
    }

    // Check for other YAML special characters that might cause issues
    // when they appear in unquoted strings
    let special_chars = [':', '{', '}', '[', ']', ',', '#'];
    for ch in special_chars {
        if value.contains(ch) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigBuilder;
    use crate::config::ConfigOverrides;
    use crate::config_loader::ConfigLayerEntry;
    use crate::config_loader::ConfigLayerStack;
    use crate::config_loader::ConfigRequirements;
    use codex_protocol::protocol::SkillScope;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use tempfile::TempDir;
    use toml::Value as TomlValue;

    const REPO_ROOT_CONFIG_DIR_NAME: &str = ".codex";

    async fn make_config(codex_home: &TempDir) -> Config {
        make_config_for_cwd(codex_home, codex_home.path().to_path_buf()).await
    }

    async fn make_config_for_cwd(codex_home: &TempDir, cwd: PathBuf) -> Config {
        let harness_overrides = ConfigOverrides {
            cwd: Some(cwd),
            ..Default::default()
        };

        ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .harness_overrides(harness_overrides)
            .build()
            .await
            .expect("defaults for test should always succeed")
    }

    fn mark_as_git_repo(dir: &Path) {
        // Config/project-root discovery only checks for the presence of `.git` (file or dir),
        // so we can avoid shelling out to `git init` in tests.
        fs::write(dir.join(".git"), "gitdir: fake\n").unwrap();
    }

    fn normalized(path: &Path) -> PathBuf {
        normalize_path(path).unwrap_or_else(|_| path.to_path_buf())
    }

    #[test]
    fn skill_roots_from_layer_stack_maps_user_to_user_and_system_cache_and_system_to_admin()
    -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;

        let system_folder = tmp.path().join("etc/codex");
        let user_folder = tmp.path().join("home/codex");
        fs::create_dir_all(&system_folder)?;
        fs::create_dir_all(&user_folder)?;

        // The file path doesn't need to exist; it's only used to derive the config folder.
        let system_file = AbsolutePathBuf::from_absolute_path(system_folder.join("config.toml"))?;
        let user_file = AbsolutePathBuf::from_absolute_path(user_folder.join("config.toml"))?;

        let layers = vec![
            ConfigLayerEntry::new(
                ConfigLayerSource::System { file: system_file },
                TomlValue::Table(toml::map::Map::new()),
            ),
            ConfigLayerEntry::new(
                ConfigLayerSource::User { file: user_file },
                TomlValue::Table(toml::map::Map::new()),
            ),
        ];
        let stack = ConfigLayerStack::new(layers, ConfigRequirements::default())?;

        let got = skill_roots_from_layer_stack(&stack)
            .into_iter()
            .map(|root| (root.scope, root.path))
            .collect::<Vec<_>>();

        assert_eq!(
            got,
            vec![
                (SkillScope::User, user_folder.join("skills")),
                (
                    SkillScope::System,
                    user_folder.join("skills").join(".system")
                ),
                (SkillScope::Admin, system_folder.join("skills")),
            ]
        );

        Ok(())
    }

    fn write_skill(codex_home: &TempDir, dir: &str, name: &str, description: &str) -> PathBuf {
        write_skill_at(&codex_home.path().join("skills"), dir, name, description)
    }

    fn write_system_skill(
        codex_home: &TempDir,
        dir: &str,
        name: &str,
        description: &str,
    ) -> PathBuf {
        write_skill_at(
            &codex_home.path().join("skills/.system"),
            dir,
            name,
            description,
        )
    }

    fn write_skill_at(root: &Path, dir: &str, name: &str, description: &str) -> PathBuf {
        let skill_dir = root.join(dir);
        fs::create_dir_all(&skill_dir).unwrap();
        let indented_description = description.replace('\n', "\n  ");
        let content = format!(
            "---\nname: {name}\ndescription: |-\n  {indented_description}\n---\n\n# Body\n"
        );
        let path = skill_dir.join(SKILLS_FILENAME);
        fs::write(&path, content).unwrap();
        path
    }

    #[tokio::test]
    async fn loads_valid_skill() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let skill_path = write_skill(&codex_home, "demo", "demo-skill", "does things\ncarefully");
        let cfg = make_config(&codex_home).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "demo-skill".to_string(),
                description: "does things carefully".to_string(),
                short_description: None,
                path: normalized(&skill_path),
                scope: SkillScope::User,
            }]
        );
    }

    #[tokio::test]
    async fn loads_short_description_from_metadata() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let skill_dir = codex_home.path().join("skills/demo");
        fs::create_dir_all(&skill_dir).unwrap();
        let contents = "---\nname: demo-skill\ndescription: long description\nmetadata:\n  short-description: short summary\n---\n\n# Body\n";
        let skill_path = skill_dir.join(SKILLS_FILENAME);
        fs::write(&skill_path, contents).unwrap();

        let cfg = make_config(&codex_home).await;
        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "demo-skill".to_string(),
                description: "long description".to_string(),
                short_description: Some("short summary".to_string()),
                path: normalized(&skill_path),
                scope: SkillScope::User,
            }]
        );
    }

    #[tokio::test]
    async fn enforces_short_description_length_limits() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let skill_dir = codex_home.path().join("skills/demo");
        fs::create_dir_all(&skill_dir).unwrap();
        let too_long = "x".repeat(MAX_SHORT_DESCRIPTION_LEN + 1);
        let contents = format!(
            "---\nname: demo-skill\ndescription: long description\nmetadata:\n  short-description: {too_long}\n---\n\n# Body\n"
        );
        fs::write(skill_dir.join(SKILLS_FILENAME), contents).unwrap();

        let cfg = make_config(&codex_home).await;
        let outcome = load_skills(&cfg);
        assert_eq!(outcome.skills.len(), 0);
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0]
                .message
                .contains("invalid metadata.short-description"),
            "expected length error, got: {:?}",
            outcome.errors
        );
    }

    #[tokio::test]
    async fn skips_hidden_and_invalid() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let hidden_dir = codex_home.path().join("skills/.hidden");
        fs::create_dir_all(&hidden_dir).unwrap();
        fs::write(
            hidden_dir.join(SKILLS_FILENAME),
            "---\nname: hidden\ndescription: hidden\n---\n",
        )
        .unwrap();

        // Invalid because missing closing frontmatter.
        let invalid_dir = codex_home.path().join("skills/invalid");
        fs::create_dir_all(&invalid_dir).unwrap();
        fs::write(invalid_dir.join(SKILLS_FILENAME), "---\nname: bad").unwrap();

        let cfg = make_config(&codex_home).await;
        let outcome = load_skills(&cfg);
        assert_eq!(outcome.skills.len(), 0);
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0]
                .message
                .contains("missing YAML frontmatter"),
            "expected frontmatter error"
        );
    }

    #[tokio::test]
    async fn enforces_length_limits() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let max_desc = "\u{1F4A1}".repeat(MAX_DESCRIPTION_LEN);
        write_skill(&codex_home, "max-len", "max-len", &max_desc);
        let cfg = make_config(&codex_home).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.skills.len(), 1);

        let too_long_desc = "\u{1F4A1}".repeat(MAX_DESCRIPTION_LEN + 1);
        write_skill(&codex_home, "too-long", "too-long", &too_long_desc);
        let outcome = load_skills(&cfg);
        assert_eq!(outcome.skills.len(), 1);
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0].message.contains("invalid description"),
            "expected length error"
        );
    }

    #[tokio::test]
    async fn loads_skills_from_repo_root() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let repo_dir = tempfile::tempdir().expect("tempdir");
        mark_as_git_repo(repo_dir.path());

        let skills_root = repo_dir
            .path()
            .join(REPO_ROOT_CONFIG_DIR_NAME)
            .join(SKILLS_DIR_NAME);
        let skill_path = write_skill_at(&skills_root, "repo", "repo-skill", "from repo");
        let cfg = make_config_for_cwd(&codex_home, repo_dir.path().to_path_buf()).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "repo-skill".to_string(),
                description: "from repo".to_string(),
                short_description: None,
                path: normalized(&skill_path),
                scope: SkillScope::Repo,
            }]
        );
    }

    #[tokio::test]
    async fn loads_skills_from_all_codex_dirs_under_project_root() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let repo_dir = tempfile::tempdir().expect("tempdir");
        mark_as_git_repo(repo_dir.path());

        let nested_dir = repo_dir.path().join("nested/inner");
        fs::create_dir_all(&nested_dir).unwrap();

        let root_skill_path = write_skill_at(
            &repo_dir
                .path()
                .join(REPO_ROOT_CONFIG_DIR_NAME)
                .join(SKILLS_DIR_NAME),
            "root",
            "root-skill",
            "from root",
        );
        let nested_skill_path = write_skill_at(
            &repo_dir
                .path()
                .join("nested")
                .join(REPO_ROOT_CONFIG_DIR_NAME)
                .join(SKILLS_DIR_NAME),
            "nested",
            "nested-skill",
            "from nested",
        );

        let cfg = make_config_for_cwd(&codex_home, nested_dir).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![
                SkillMetadata {
                    name: "nested-skill".to_string(),
                    description: "from nested".to_string(),
                    short_description: None,
                    path: normalized(&nested_skill_path),
                    scope: SkillScope::Repo,
                },
                SkillMetadata {
                    name: "root-skill".to_string(),
                    description: "from root".to_string(),
                    short_description: None,
                    path: normalized(&root_skill_path),
                    scope: SkillScope::Repo,
                },
            ]
        );
    }

    #[tokio::test]
    async fn loads_skills_from_codex_dir_when_not_git_repo() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let work_dir = tempfile::tempdir().expect("tempdir");

        let skill_path = write_skill_at(
            &work_dir
                .path()
                .join(REPO_ROOT_CONFIG_DIR_NAME)
                .join(SKILLS_DIR_NAME),
            "local",
            "local-skill",
            "from cwd",
        );

        let cfg = make_config_for_cwd(&codex_home, work_dir.path().to_path_buf()).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "local-skill".to_string(),
                description: "from cwd".to_string(),
                short_description: None,
                path: normalized(&skill_path),
                scope: SkillScope::Repo,
            }]
        );
    }

    #[tokio::test]
    async fn deduplicates_by_name_preferring_repo_over_user() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let repo_dir = tempfile::tempdir().expect("tempdir");
        mark_as_git_repo(repo_dir.path());

        let _user_skill_path = write_skill(&codex_home, "user", "dupe-skill", "from user");
        let repo_skill_path = write_skill_at(
            &repo_dir
                .path()
                .join(REPO_ROOT_CONFIG_DIR_NAME)
                .join(SKILLS_DIR_NAME),
            "repo",
            "dupe-skill",
            "from repo",
        );

        let cfg = make_config_for_cwd(&codex_home, repo_dir.path().to_path_buf()).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "dupe-skill".to_string(),
                description: "from repo".to_string(),
                short_description: None,
                path: normalized(&repo_skill_path),
                scope: SkillScope::Repo,
            }]
        );
    }

    #[tokio::test]
    async fn loads_system_skills_when_present() {
        let codex_home = tempfile::tempdir().expect("tempdir");

        let _system_skill_path =
            write_system_skill(&codex_home, "system", "dupe-skill", "from system");
        let user_skill_path = write_skill(&codex_home, "user", "dupe-skill", "from user");

        let cfg = make_config(&codex_home).await;
        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "dupe-skill".to_string(),
                description: "from user".to_string(),
                short_description: None,
                path: normalized(&user_skill_path),
                scope: SkillScope::User,
            }]
        );
    }

    #[tokio::test]
    async fn repo_skills_search_does_not_escape_repo_root() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let outer_dir = tempfile::tempdir().expect("tempdir");
        let repo_dir = outer_dir.path().join("repo");
        fs::create_dir_all(&repo_dir).unwrap();

        write_skill_at(
            &outer_dir
                .path()
                .join(REPO_ROOT_CONFIG_DIR_NAME)
                .join(SKILLS_DIR_NAME),
            "outer",
            "outer-skill",
            "from outer",
        );

        mark_as_git_repo(&repo_dir);

        let cfg = make_config_for_cwd(&codex_home, repo_dir).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.skills.len(), 0);
    }

    #[tokio::test]
    async fn loads_skills_when_cwd_is_file_in_repo() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let repo_dir = tempfile::tempdir().expect("tempdir");
        mark_as_git_repo(repo_dir.path());

        let skill_path = write_skill_at(
            &repo_dir
                .path()
                .join(REPO_ROOT_CONFIG_DIR_NAME)
                .join(SKILLS_DIR_NAME),
            "repo",
            "repo-skill",
            "from repo",
        );
        let file_path = repo_dir.path().join("some-file.txt");
        fs::write(&file_path, "contents").unwrap();

        let cfg = make_config_for_cwd(&codex_home, file_path).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "repo-skill".to_string(),
                description: "from repo".to_string(),
                short_description: None,
                path: normalized(&skill_path),
                scope: SkillScope::Repo,
            }]
        );
    }

    #[tokio::test]
    async fn non_git_repo_skills_search_does_not_walk_parents() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let outer_dir = tempfile::tempdir().expect("tempdir");
        let nested_dir = outer_dir.path().join("nested/inner");
        fs::create_dir_all(&nested_dir).unwrap();

        write_skill_at(
            &outer_dir
                .path()
                .join(REPO_ROOT_CONFIG_DIR_NAME)
                .join(SKILLS_DIR_NAME),
            "outer",
            "outer-skill",
            "from outer",
        );

        let cfg = make_config_for_cwd(&codex_home, nested_dir).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.skills.len(), 0);
    }

    #[tokio::test]
    async fn loads_skills_from_system_cache_when_present() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let work_dir = tempfile::tempdir().expect("tempdir");

        let skill_path = write_system_skill(&codex_home, "system", "system-skill", "from system");

        let cfg = make_config_for_cwd(&codex_home, work_dir.path().to_path_buf()).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "system-skill".to_string(),
                description: "from system".to_string(),
                short_description: None,
                path: normalized(&skill_path),
                scope: SkillScope::System,
            }]
        );
    }

    #[tokio::test]
    async fn skill_roots_include_admin_with_lowest_priority_on_unix() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let cfg = make_config(&codex_home).await;

        let scopes: Vec<SkillScope> = skill_roots(&cfg)
            .into_iter()
            .map(|root| root.scope)
            .collect();
        let mut expected = vec![SkillScope::User, SkillScope::System];
        if cfg!(unix) {
            expected.push(SkillScope::Admin);
        }
        assert_eq!(scopes, expected);
    }

    #[tokio::test]
    async fn deduplicates_by_name_preferring_system_over_admin() {
        let system_dir = tempfile::tempdir().expect("tempdir");
        let admin_dir = tempfile::tempdir().expect("tempdir");

        let system_skill_path =
            write_skill_at(system_dir.path(), "system", "dupe-skill", "from system");
        let _admin_skill_path =
            write_skill_at(admin_dir.path(), "admin", "dupe-skill", "from admin");

        let outcome = load_skills_from_roots([
            SkillRoot {
                path: system_dir.path().to_path_buf(),
                scope: SkillScope::System,
            },
            SkillRoot {
                path: admin_dir.path().to_path_buf(),
                scope: SkillScope::Admin,
            },
        ]);

        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "dupe-skill".to_string(),
                description: "from system".to_string(),
                short_description: None,
                path: normalized(&system_skill_path),
                scope: SkillScope::System,
            }]
        );
    }

    #[tokio::test]
    async fn deduplicates_by_name_preferring_user_over_system() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let work_dir = tempfile::tempdir().expect("tempdir");

        let user_skill_path = write_skill(&codex_home, "user", "dupe-skill", "from user");
        let _system_skill_path =
            write_system_skill(&codex_home, "system", "dupe-skill", "from system");

        let cfg = make_config_for_cwd(&codex_home, work_dir.path().to_path_buf()).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "dupe-skill".to_string(),
                description: "from user".to_string(),
                short_description: None,
                path: normalized(&user_skill_path),
                scope: SkillScope::User,
            }]
        );
    }

    #[tokio::test]
    async fn deduplicates_by_name_preferring_repo_over_system() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let repo_dir = tempfile::tempdir().expect("tempdir");
        mark_as_git_repo(repo_dir.path());

        let repo_skill_path = write_skill_at(
            &repo_dir
                .path()
                .join(REPO_ROOT_CONFIG_DIR_NAME)
                .join(SKILLS_DIR_NAME),
            "repo",
            "dupe-skill",
            "from repo",
        );
        let _system_skill_path =
            write_system_skill(&codex_home, "system", "dupe-skill", "from system");

        let cfg = make_config_for_cwd(&codex_home, repo_dir.path().to_path_buf()).await;

        let outcome = load_skills(&cfg);
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "dupe-skill".to_string(),
                description: "from repo".to_string(),
                short_description: None,
                path: normalized(&repo_skill_path),
                scope: SkillScope::Repo,
            }]
        );
    }

    #[tokio::test]
    async fn deduplicates_by_name_preferring_nearest_project_codex_dir() {
        let codex_home = tempfile::tempdir().expect("tempdir");
        let repo_dir = tempfile::tempdir().expect("tempdir");
        mark_as_git_repo(repo_dir.path());

        let nested_dir = repo_dir.path().join("nested/inner");
        fs::create_dir_all(&nested_dir).unwrap();

        let _root_skill_path = write_skill_at(
            &repo_dir
                .path()
                .join(REPO_ROOT_CONFIG_DIR_NAME)
                .join(SKILLS_DIR_NAME),
            "root",
            "dupe-skill",
            "from root",
        );
        let nested_skill_path = write_skill_at(
            &repo_dir
                .path()
                .join("nested")
                .join(REPO_ROOT_CONFIG_DIR_NAME)
                .join(SKILLS_DIR_NAME),
            "nested",
            "dupe-skill",
            "from nested",
        );

        let cfg = make_config_for_cwd(&codex_home, nested_dir).await;
        let outcome = load_skills(&cfg);

        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        let expected_path =
            normalize_path(&nested_skill_path).unwrap_or_else(|_| nested_skill_path.clone());
        assert_eq!(
            vec![SkillMetadata {
                name: "dupe-skill".to_string(),
                description: "from nested".to_string(),
                short_description: None,
                path: expected_path,
                scope: SkillScope::Repo,
            }],
            outcome.skills
        );
    }

    #[test]
    fn test_normalize_frontmatter_yaml() {
        // Test that colons in values are properly handled
        let input = "name: test-skill\ndescription: Use when prompt: `pattern` appears";
        let output = normalize_frontmatter_yaml(input);
        assert!(
            output.contains("description: \"Use when prompt: `pattern` appears\""),
            "Expected quoted description, got: {}",
            output
        );

        // Test that backslashes and colons are properly escaped
        let input = "name: test\ndescription: Use when prompt: `\\d+` appears";
        let output = normalize_frontmatter_yaml(input);
        assert!(
            output.contains(r#"description: "Use when prompt: `\\d+` appears""#),
            "Expected escaped backslashes, got: {}",
            output
        );

        // Test that already quoted values are not double-quoted
        let input = "name: test\ndescription: \"already quoted: value\"";
        let output = normalize_frontmatter_yaml(input);
        assert_eq!(input, output, "Already quoted values should not change");

        // Test that block scalar syntax is preserved
        let input = "name: test\ndescription: |-\n  multiline\n  value: with colon";
        let output = normalize_frontmatter_yaml(input);
        assert_eq!(input, output, "Block scalar syntax should be preserved");

        // Test that simple values without special chars are unchanged
        let input = "name: test-skill\ndescription: A simple description";
        let output = normalize_frontmatter_yaml(input);
        assert_eq!(input, output, "Simple values should not change");
    }

    #[tokio::test]
    async fn accepts_description_with_unquoted_special_yaml_chars() {
        // Tests that the parser automatically handles descriptions containing colons
        // like "prompt: `PLS-\d+`" even without explicit quoting.
        // This is the exact content that used to cause YAML parsing errors before
        // the automatic normalization was added.
        let codex_home = tempfile::tempdir().expect("tempdir");
        let skill_dir = codex_home.path().join("skills/address-bug");
        fs::create_dir_all(&skill_dir).unwrap();

        let contents = "---\nname: address-bug\ndescription: Addresses technical bugs from an issue tracker. Should be used when the following RegExp is present in a prompt: `PLS-\\d+`.\n---\n\n# Body\n";
        let skill_path = skill_dir.join(SKILLS_FILENAME);
        fs::write(&skill_path, contents).unwrap();

        let cfg = make_config(&codex_home).await;
        let outcome = load_skills(&cfg);

        // The skill should now load successfully thanks to automatic normalization
        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.skills.len(), 1);
        assert_eq!(outcome.skills[0].name, "address-bug");
        // Verify the description was parsed correctly (note: backslash is preserved in parsed value)
        assert!(
            outcome.skills[0]
                .description
                .contains("RegExp is present in a prompt: `PLS-\\d+`"),
            "Description should contain the pattern, got: {:?}",
            outcome.skills[0].description
        );
    }

    #[tokio::test]
    async fn accepts_description_with_manually_quoted_yaml() {
        // Tests that manually quoted descriptions with proper YAML escaping work correctly
        let codex_home = tempfile::tempdir().expect("tempdir");
        let skill_dir = codex_home.path().join("skills/address-bug");
        fs::create_dir_all(&skill_dir).unwrap();

        // Using double quotes with proper YAML escaping (backslashes must be doubled)
        let contents = "---\nname: address-bug\ndescription: \"Addresses technical bugs from an issue tracker. Should be used when the following RegExp is present in a prompt: `PLS-\\\\d+`.\"\n---\n\n# Body\n";
        let skill_path = skill_dir.join(SKILLS_FILENAME);
        fs::write(&skill_path, contents).unwrap();

        let cfg = make_config(&codex_home).await;
        let outcome = load_skills(&cfg);

        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(
            outcome.skills,
            vec![SkillMetadata {
                name: "address-bug".to_string(),
                description: "Addresses technical bugs from an issue tracker. Should be used when the following RegExp is present in a prompt: `PLS-\\d+`.".to_string(),
                short_description: None,
                path: normalized(&skill_path),
                scope: SkillScope::User,
            }]
        );
    }

    #[tokio::test]
    async fn accepts_description_with_block_scalar_yaml() {
        // Using block scalar syntax (|-) is the recommended approach for complex descriptions
        let codex_home = tempfile::tempdir().expect("tempdir");
        let skill_dir = codex_home.path().join("skills/address-bug");
        fs::create_dir_all(&skill_dir).unwrap();

        // Using block scalar syntax which is more robust
        let contents = "---\nname: address-bug\ndescription: |-\n  Addresses technical bugs from an issue tracker. Should be used when the following\n  RegExp is present in a prompt: `PLS-\\d+`.\n---\n\n# Body\n";
        let skill_path = skill_dir.join(SKILLS_FILENAME);
        fs::write(&skill_path, contents).unwrap();

        let cfg = make_config(&codex_home).await;
        let outcome = load_skills(&cfg);

        assert!(
            outcome.errors.is_empty(),
            "unexpected errors: {:?}",
            outcome.errors
        );
        assert_eq!(outcome.skills.len(), 1);
        assert_eq!(outcome.skills[0].name, "address-bug");
        // The description should be sanitized (single line with whitespace normalized)
        assert!(outcome.skills[0]
            .description
            .contains("RegExp is present in a prompt: `PLS-\\d+`."));
    }
}
