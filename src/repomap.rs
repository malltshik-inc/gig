use std::path::Path;

use anyhow::Result;
use ignore::WalkBuilder;

use crate::lang::{self, SymbolInfo};

pub struct FileEntry {
    pub path: String,
    pub symbols: Vec<SymbolInfo>,
    pub score: i64,
}

pub struct RepoMap {
    pub files: Vec<FileEntry>,
    pub total_files: usize,
    pub total_symbols: usize,
    pub truncated: bool,
}

pub fn build(root: &Path, prompt: &str, max_files: usize) -> Result<RepoMap> {
    let terms = tokenize(prompt);
    let mut entries = Vec::new();
    let mut total_files = 0usize;
    let mut total_symbols = 0usize;

    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_filesize(Some(1_000_000))
        .build();

    for result in walker {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.starts_with("target/") || relative.contains("/target/") {
            continue;
        }
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(_) => continue,
        };
        let symbols = match lang::list_symbols(&source) {
            Ok(symbols) => symbols,
            Err(_) => continue,
        };
        total_files += 1;
        total_symbols += symbols.len();
        let score = score_file(&relative, &symbols, &terms);
        entries.push(FileEntry {
            path: relative,
            symbols,
            score,
        });
    }

    // The Cargo.toml manifest is editable as a whole file; list it so the
    // planner may legally target it for tasks such as version bumps.
    let manifest = root.join("Cargo.toml");
    if manifest.is_file() {
        entries.push(FileEntry {
            path: "Cargo.toml".to_string(),
            symbols: Vec::new(),
            score: i64::MAX,
        });
    }

    entries.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path.len().cmp(&right.path.len()))
            .then_with(|| left.path.cmp(&right.path))
    });

    let truncated = entries.len() > max_files;
    entries.truncate(max_files);

    Ok(RepoMap {
        files: entries,
        total_files,
        total_symbols,
        truncated,
    })
}

fn score_file(path: &str, symbols: &[SymbolInfo], terms: &[String]) -> i64 {
    let lower_path = path.to_lowercase();
    let mut score = 0i64;
    for term in terms {
        if lower_path.contains(term) {
            score += 8;
        }
        for symbol in symbols {
            let name = symbol.name.to_lowercase();
            if name == *term {
                score += 30;
            } else if name.contains(term) {
                score += 12;
            }
            if symbol.qualified.to_lowercase().contains(term) {
                score += 4;
            }
        }
    }
    score -= path.matches('/').count() as i64;
    score
}

pub fn tokenize(text: &str) -> Vec<String> {
    // Only grammatical noise is removed. Task words such as "add", "fix", or
    // "test" are often symbol names and must still score.
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "this", "that", "from", "into", "when", "then", "than",
        "have", "has", "are", "was", "were", "you", "your", "our", "its", "please", "should",
        "would", "could",
    ];

    let mut tokens = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if raw.is_empty() {
            continue;
        }
        for part in split_identifier(raw) {
            let token = part.to_lowercase();
            if token.len() >= 3 && !STOPWORDS.contains(&token.as_str()) && !tokens.contains(&token)
            {
                tokens.push(token);
            }
        }
    }
    tokens
}

fn split_identifier(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for chunk in value.split('_') {
        let mut current = String::new();
        for character in chunk.chars() {
            if character.is_uppercase() && !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            current.push(character);
        }
        if !current.is_empty() {
            parts.push(current);
        }
    }
    parts
}

impl RepoMap {
    pub fn render(&self, max_chars: usize) -> String {
        let mut output = String::new();
        output.push_str(&format!(
            "{} files indexed, {} symbols. Indented symbols belong to the file above them.\n",
            self.total_files, self.total_symbols
        ));
        if self.truncated {
            output.push_str("(map truncated to the most relevant files)\n");
        }
        for file in &self.files {
            let mut block = String::new();
            block.push_str(&file.path);
            block.push('\n');
            for symbol in file.symbols.iter().take(14) {
                block.push_str(&format!("  {} ({})\n", symbol.qualified, symbol.kind));
            }
            if file.symbols.len() > 14 {
                block.push_str(&format!("  +{} more\n", file.symbols.len() - 14));
            }
            if output.len() + block.len() > max_chars {
                output.push_str("(remaining files omitted)\n");
                break;
            }
            output.push_str(&block);
        }
        if self.files.is_empty() {
            output.push_str("(no Rust files found)\n");
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_snake_and_camel_case() {
        let tokens = tokenize("Fix the login_timeout in AuthService, please");
        assert!(tokens.contains(&"login".to_string()));
        assert!(tokens.contains(&"timeout".to_string()));
        assert!(tokens.contains(&"auth".to_string()));
        assert!(tokens.contains(&"service".to_string()));
        assert!(!tokens.contains(&"the".to_string()));
    }

    #[test]
    fn keeps_task_words_that_are_often_symbol_names() {
        let tokens = tokenize("Fix the add function and update the test");
        assert!(tokens.contains(&"add".to_string()));
        assert!(tokens.contains(&"fix".to_string()));
        assert!(tokens.contains(&"update".to_string()));
        assert!(tokens.contains(&"test".to_string()));
    }

    #[test]
    fn ranks_files_with_matching_symbols_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/auth.rs"),
            "pub fn login_timeout() -> u64 { 30 }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/math.rs"),
            "pub fn add(a: i32) -> i32 { a + 1 }\n",
        )
        .unwrap();

        let map = build(dir.path(), "fix the login timeout", 5).unwrap();
        assert_eq!(map.files[0].path, "src/auth.rs");
    }

    #[test]
    fn lists_the_manifest_for_whole_file_edits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();

        let map = build(dir.path(), "bump the version", 5).unwrap();
        assert_eq!(map.files[0].path, "Cargo.toml");
        assert!(map.render(1000).contains("Cargo.toml"));
    }
}
