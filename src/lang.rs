use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use tree_sitter::{Node, Parser, Tree};

#[derive(Clone, Debug)]
#[allow(dead_code)] // metadata is used by the repo map and kept for future UI/retrieval
pub struct SymbolInfo {
    pub name: String,
    pub qualified: String,
    pub kind: &'static str,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug)]
pub struct ResolvedSymbol {
    pub qualified: String,
    pub kind: &'static str,
    /// Byte offset where the replacement starts: the beginning of the symbol's
    /// line, including indentation. Preceding attributes and doc comments are
    /// kept outside this range and preserved automatically.
    pub start_byte: usize,
    pub end_byte: usize,
    pub indent: String,
    /// The symbol plus its preceding attributes and doc comments, for the model.
    pub source: String,
    /// The attribute/doc-comment prefix that `apply` preserves.
    pub prefix: String,
}

struct Candidate<'a> {
    name: String,
    qualified: String,
    kind: &'static str,
    node: Node<'a>,
}

fn parser() -> Result<Parser> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .context("failed to load the tree-sitter-rust grammar")?;
    Ok(parser)
}

fn parse(source: &str) -> Result<Tree> {
    let mut parser = parser()?;
    parser
        .parse(source, None)
        .context("tree-sitter failed to parse the source")
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn qualify(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}::{name}")
    }
}

fn collect<'a>(root: Node<'a>, source: &str, prefix: &str, out: &mut Vec<Candidate<'a>>) {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        collect_item(child, source, prefix, out);
    }
}

fn collect_item<'a>(node: Node<'a>, source: &str, prefix: &str, out: &mut Vec<Candidate<'a>>) {
    match node.kind() {
        "function_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(name_node, source).to_string();
                out.push(Candidate {
                    qualified: qualify(prefix, &name),
                    name,
                    kind: "function",
                    node,
                });
            }
        }
        "struct_item" | "enum_item" | "union_item" | "trait_item" | "type_item" | "const_item"
        | "static_item" | "macro_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(name_node, source).to_string();
                let kind = match node.kind() {
                    "struct_item" => "struct",
                    "enum_item" => "enum",
                    "union_item" => "union",
                    "trait_item" => "trait",
                    "type_item" => "type",
                    "const_item" => "const",
                    "static_item" => "static",
                    _ => "macro",
                };
                out.push(Candidate {
                    qualified: qualify(prefix, &name),
                    name: name.clone(),
                    kind,
                    node,
                });
                if node.kind() == "trait_item"
                    && let Some(body) = node.child_by_field_name("body")
                {
                    let mut cursor = body.walk();
                    for child in body.named_children(&mut cursor) {
                        if matches!(child.kind(), "function_item" | "function_signature_item")
                            && let Some(method_name) = child.child_by_field_name("name")
                        {
                            let method = text(method_name, source).to_string();
                            out.push(Candidate {
                                qualified: format!("{}::{method}", qualify(prefix, &name)),
                                name: method,
                                kind: "method",
                                node: child,
                            });
                        }
                    }
                }
            }
        }
        "impl_item" => {
            let type_name = node
                .child_by_field_name("type")
                .map(|n| text(n, source).to_string())
                .unwrap_or_else(|| "_".to_string());
            let qualified = qualify(prefix, &type_name);
            out.push(Candidate {
                name: type_name.clone(),
                qualified: qualified.clone(),
                kind: "impl",
                node,
            });
            if let Some(body) = node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.named_children(&mut cursor) {
                    if child.kind() == "function_item"
                        && let Some(name_node) = child.child_by_field_name("name")
                    {
                        let name = text(name_node, source).to_string();
                        out.push(Candidate {
                            qualified: format!("{qualified}::{name}"),
                            name,
                            kind: "method",
                            node: child,
                        });
                    }
                }
            }
        }
        "mod_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = text(name_node, source).to_string();
                let qualified = qualify(prefix, &name);
                out.push(Candidate {
                    name,
                    qualified: qualified.clone(),
                    kind: "module",
                    node,
                });
                if let Some(body) = node.child_by_field_name("body") {
                    collect(body, source, &qualified, out);
                }
            }
        }
        _ => {}
    }
}

pub fn list_symbols(source: &str) -> Result<Vec<SymbolInfo>> {
    let tree = parse(source)?;
    let mut candidates = Vec::new();
    collect(tree.root_node(), source, "", &mut candidates);
    Ok(candidates
        .into_iter()
        .map(|candidate| SymbolInfo {
            name: candidate.name,
            qualified: candidate.qualified,
            kind: candidate.kind,
            start_line: candidate.node.start_position().row + 1,
            end_line: candidate.node.end_position().row + 1,
        })
        .collect())
}

fn pick<'a>(candidates: &[&'a Candidate<'a>]) -> Option<&'a Candidate<'a>> {
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }
    let non_impl: Vec<&&Candidate<'_>> = candidates.iter().filter(|c| c.kind != "impl").collect();
    if non_impl.len() == 1 {
        return Some(non_impl[0]);
    }
    None
}

pub fn resolve(source: &str, requested: &str) -> Result<ResolvedSymbol> {
    let requested = requested.trim();
    if requested.is_empty() {
        bail!("empty symbol name");
    }
    let tree = parse(source)?;
    let mut candidates = Vec::new();
    collect(tree.root_node(), source, "", &mut candidates);

    let exact: Vec<&Candidate<'_>> = candidates
        .iter()
        .filter(|candidate| candidate.qualified == requested)
        .collect();
    let by_name: Vec<&Candidate<'_>> = candidates
        .iter()
        .filter(|candidate| candidate.name == requested)
        .collect();

    let chosen = if !exact.is_empty() {
        pick(&exact)
    } else if !by_name.is_empty() {
        pick(&by_name)
    } else {
        let suffix: Vec<&Candidate<'_>> = candidates
            .iter()
            .filter(|candidate| candidate.qualified.ends_with(&format!("::{requested}")))
            .collect();
        pick(&suffix)
    };

    let chosen = match chosen {
        Some(candidate) => candidate,
        None => {
            let mut names: Vec<String> = candidates
                .iter()
                .filter(|candidate| {
                    candidate.name == requested
                        || candidate.qualified == requested
                        || candidate.qualified.ends_with(&format!("::{requested}"))
                })
                .map(|candidate| format!("{} ({})", candidate.qualified, candidate.kind))
                .collect();
            names.sort();
            names.dedup();
            if names.is_empty() {
                bail!("symbol `{requested}` was not found in the file");
            }
            bail!("symbol `{requested}` is ambiguous: {}", names.join(", "));
        }
    };

    let prefix_start = extend_start(source, chosen.node);
    let prefix_line_start = source[..prefix_start]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let prefix_indent = &source[prefix_line_start..prefix_start];
    let (prefix_start_byte, indent) = if prefix_indent.chars().all(|c| c == ' ' || c == '\t') {
        (prefix_line_start, prefix_indent.to_string())
    } else {
        (prefix_start, String::new())
    };

    let node_line_start = source[..chosen.node.start_byte()]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let node_indent = &source[node_line_start..chosen.node.start_byte()];
    let start_byte = if node_indent.chars().all(|c| c == ' ' || c == '\t') {
        node_line_start
    } else {
        chosen.node.start_byte()
    };
    let end_byte = chosen.node.end_byte();

    Ok(ResolvedSymbol {
        qualified: chosen.qualified.clone(),
        kind: chosen.kind,
        start_byte,
        end_byte,
        indent,
        source: source[prefix_start_byte..end_byte].to_string(),
        prefix: source[prefix_start_byte..start_byte].to_string(),
    })
}

fn extend_start(source: &str, node: Node<'_>) -> usize {
    let mut start = node.start_byte();
    let mut previous = node.prev_sibling();
    while let Some(prev) = previous {
        if matches!(
            prev.kind(),
            "attribute_item" | "line_comment" | "block_comment"
        ) {
            let gap = &source[prev.end_byte()..start];
            if gap.chars().all(char::is_whitespace) && gap.matches('\n').count() <= 1 {
                start = prev.start_byte();
                previous = prev.prev_sibling();
                continue;
            }
        }
        break;
    }
    start
}

pub fn apply(
    source: &str,
    resolved: &ResolvedSymbol,
    new_code: &str,
    new_imports: &[String],
) -> Result<String> {
    let eol = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let cleaned = strip_duplicate_prefix(new_code, &resolved.prefix);
    let replacement = reindent(&cleaned, &resolved.indent, eol);
    if replacement.trim().is_empty() {
        bail!("the replacement for `{}` is empty", resolved.qualified);
    }
    let mut output = String::with_capacity(source.len() + replacement.len());
    output.push_str(&source[..resolved.start_byte]);
    output.push_str(&replacement);
    output.push_str(&source[resolved.end_byte..]);
    if !new_imports.is_empty() {
        output = insert_imports(&output, new_imports)?;
    }
    Ok(output)
}

/// If the model repeats attributes or doc comments that already sit outside
/// the replacement range, drop those lines instead of duplicating them.
fn strip_duplicate_prefix(code: &str, prefix: &str) -> String {
    let prefix_lines: BTreeSet<&str> = prefix
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if prefix_lines.is_empty() {
        return code.to_string();
    }

    let normalized = code.replace("\r\n", "\n");
    let mut lines: Vec<&str> = normalized.lines().collect();
    while let Some(first) = lines.first() {
        let trimmed = first.trim();
        let is_meta = trimmed.starts_with("#[")
            || trimmed.starts_with("///")
            || trimmed.starts_with("//!")
            || trimmed.starts_with("//")
            || trimmed.starts_with("/**")
            || trimmed.starts_with('*');
        if is_meta && prefix_lines.contains(trimmed) {
            lines.remove(0);
        } else {
            break;
        }
    }
    lines.join("\n")
}

fn reindent(code: &str, indent: &str, eol: &str) -> String {
    let normalized = code.replace("\r\n", "\n");
    let trimmed = normalized.trim_matches('\n');
    let dedented = dedent(trimmed);
    let mut lines = Vec::new();
    for line in dedented.split('\n') {
        if line.trim().is_empty() {
            lines.push(String::new());
        } else {
            lines.push(format!("{indent}{line}"));
        }
    }
    lines.join(eol).trim_end().to_string()
}

fn dedent(code: &str) -> String {
    let minimum = code
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    code.lines()
        .map(|line| {
            if line.len() >= minimum {
                &line[minimum..]
            } else {
                line.trim_start()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn insert_imports(source: &str, imports: &[String]) -> Result<String> {
    let tree = parse(source)?;
    let root = tree.root_node();
    let eol = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };

    let mut last_use_end = None;
    let mut last_inner_attribute_end = None;
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "use_declaration" => last_use_end = Some(child.end_byte()),
            "inner_attribute_item" => last_inner_attribute_end = Some(child.end_byte()),
            "line_comment" if text(child, source).trim_start().starts_with("//!") => {
                last_inner_attribute_end = Some(child.end_byte());
            }
            "block_comment" if text(child, source).trim_start().starts_with("/*!") => {
                last_inner_attribute_end = Some(child.end_byte());
            }
            _ => {}
        }
    }

    let filtered: Vec<String> = imports
        .iter()
        .map(|line| line.trim().trim_end_matches(';').trim().to_string())
        .filter(|line| !line.is_empty())
        .filter(|line| {
            !source
                .lines()
                .any(|existing| existing.trim().trim_end_matches(';').trim() == line)
        })
        .map(|line| format!("{line};"))
        .collect();

    if filtered.is_empty() {
        return Ok(source.to_string());
    }

    let block = filtered.join(eol);
    let insert_at = last_use_end.or(last_inner_attribute_end).unwrap_or(0);

    if insert_at == 0 {
        let mut output = String::with_capacity(source.len() + block.len() + 2);
        output.push_str(&block);
        output.push_str(eol);
        output.push_str(eol);
        output.push_str(source);
        return Ok(output);
    }

    let mut output = String::with_capacity(source.len() + block.len() + 1);
    output.push_str(&source[..insert_at]);
    output.push_str(eol);
    output.push_str(&block);
    output.push_str(&source[insert_at..]);
    Ok(output)
}

/// A module file the harness can link into the module tree deterministically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleDecl {
    /// The file that should contain `mod <module>;`.
    pub parent: PathBuf,
    /// The module name to declare.
    pub module: String,
}

/// Given a newly created `.rs` file, work out which existing file must declare
/// it and under what name. Returns `None` for crate roots, binaries, and files
/// whose parent module does not exist.
pub fn module_declaration(root: &Path, relative: &Path) -> Option<ModuleDecl> {
    let normalized = relative.to_string_lossy().replace('\\', "/");
    let rest = normalized.strip_prefix("src/")?;
    if rest == "lib.rs" || rest == "main.rs" || rest.starts_with("bin/") {
        return None;
    }

    let path = Path::new(rest);
    let file_name = path.file_name()?.to_str()?;
    let (module, directory) = if file_name == "mod.rs" {
        let directory = path.parent()?;
        if directory.as_os_str().is_empty() {
            return None;
        }
        let module = directory.file_name()?.to_str()?.to_string();
        let parent = directory.parent().unwrap_or(Path::new(""));
        (module, parent.to_path_buf())
    } else {
        let module = path.file_stem()?.to_str()?.to_string();
        (module, path.parent()?.to_path_buf())
    };

    let parent = if directory.as_os_str().is_empty() {
        if root.join("src/lib.rs").exists() {
            PathBuf::from("src/lib.rs")
        } else if root.join("src/main.rs").exists() {
            PathBuf::from("src/main.rs")
        } else {
            return None;
        }
    } else {
        let directory = directory.to_string_lossy().replace('\\', "/");
        let sibling = PathBuf::from(format!("src/{directory}.rs"));
        let nested = PathBuf::from(format!("src/{directory}/mod.rs"));
        if root.join(&sibling).exists() {
            sibling
        } else if root.join(&nested).exists() {
            nested
        } else {
            return None;
        }
    };

    Some(ModuleDecl { parent, module })
}

/// True when `source` does not already declare `module`.
pub fn module_missing(source: &str, module: &str) -> bool {
    let target = format!("mod {module};");
    !source.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.ends_with(&target)
            && (trimmed.starts_with("mod ")
                || trimmed.starts_with("pub mod ")
                || trimmed.starts_with("pub(crate) mod ")
                || trimmed.starts_with("pub(super) mod "))
    })
}

/// Insert `mod <module>;` after the leading imports.
pub fn add_module(source: &str, module: &str) -> Result<String> {
    insert_imports(source, &[format!("mod {module};")])
}

pub fn error_count(source: &str) -> Result<usize> {
    let tree = parse(source)?;
    Ok(count_errors(tree.root_node()))
}

fn count_errors(node: Node<'_>) -> usize {
    let mut count = if node.is_error() || node.is_missing() {
        1
    } else {
        0
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count += count_errors(child);
    }
    count
}

fn find_error<'a>(node: Node<'a>) -> Option<Node<'a>> {
    if node.is_error() || node.is_missing() {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(error) = find_error(child) {
            return Some(error);
        }
    }
    None
}

pub fn first_error_summary(source: &str) -> String {
    let tree = match parse(source) {
        Ok(tree) => tree,
        Err(error) => return format!("{error:#}"),
    };
    match find_error(tree.root_node()) {
        Some(node) => format!(
            "line {}, column {}: unexpected `{}`",
            node.start_position().row + 1,
            node.start_position().column + 1,
            node.kind()
        ),
        None => "unknown syntax error".to_string(),
    }
}

pub fn rustfmt_clean(path: &Path, edition: &str) -> bool {
    Command::new("rustfmt")
        .arg("--edition")
        .arg(edition)
        .arg("--check")
        .arg(path)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(true)
}

pub fn rustfmt(path: &Path, edition: &str) -> Result<()> {
    let output = Command::new("rustfmt")
        .arg("--edition")
        .arg(edition)
        .arg(path)
        .output()
        .with_context(|| format!("failed to run rustfmt on {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "rustfmt failed on {}:\n{}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
use std::collections::HashMap;

/// Adds one.
#[inline]
pub fn add_one(x: i32) -> i32 {
    x + 1
}

pub struct Counter {
    value: i32,
}

impl Counter {
    pub fn increment(&mut self) {
        self.value += 1;
    }

    #[cfg(test)]
    fn reset(&mut self) {
        self.value = 0;
    }
}

#[test]
fn it_works() {
    assert_eq!(add_one(1), 2);
}

mod inner {
    pub fn helper() -> u8 { 1 }
}
"#;

    #[test]
    fn lists_top_level_and_nested_symbols() {
        let symbols = list_symbols(SAMPLE).unwrap();
        let qualified: Vec<&str> = symbols.iter().map(|s| s.qualified.as_str()).collect();
        assert!(qualified.contains(&"add_one"));
        assert!(qualified.contains(&"Counter"));
        assert!(qualified.contains(&"Counter::increment"));
        assert!(qualified.contains(&"Counter::reset"));
        assert!(qualified.contains(&"it_works"));
        assert!(qualified.contains(&"inner::helper"));
    }

    #[test]
    fn resolves_function_with_attributes_and_docs() {
        let resolved = resolve(SAMPLE, "add_one").unwrap();
        assert!(resolved.source.contains("/// Adds one."));
        assert!(resolved.source.contains("#[inline]"));
        assert!(resolved.source.contains("pub fn add_one"));
        assert_eq!(resolved.kind, "function");
    }

    #[test]
    fn resolves_method_with_indent() {
        let resolved = resolve(SAMPLE, "Counter::increment").unwrap();
        assert_eq!(resolved.indent, "    ");
        assert!(resolved.source.trim_start().starts_with("pub fn increment"));
    }

    #[test]
    fn resolves_method_by_short_name() {
        let resolved = resolve(SAMPLE, "increment").unwrap();
        assert_eq!(resolved.qualified, "Counter::increment");
    }

    #[test]
    fn replaces_function_and_keeps_attributes() {
        let resolved = resolve(SAMPLE, "add_one").unwrap();
        let new_code = "pub fn add_one(x: i32) -> i32 {\n    x + 2\n}";
        let updated = apply(SAMPLE, &resolved, new_code, &[]).unwrap();
        assert!(updated.contains("x + 2"));
        assert!(updated.contains("/// Adds one."));
        assert!(updated.contains("#[inline]"));
        assert_eq!(error_count(&updated).unwrap(), 0);
    }

    #[test]
    fn reindents_method_bodies() {
        let resolved = resolve(SAMPLE, "Counter::increment").unwrap();
        let new_code = "pub fn increment(&mut self) {\n    self.value += 2;\n}";
        let updated = apply(SAMPLE, &resolved, new_code, &[]).unwrap();
        assert!(updated.contains("        self.value += 2;"));
        assert_eq!(error_count(&updated).unwrap(), 0);
    }

    #[test]
    fn inserts_module_declarations_after_inner_docs() {
        let source = "//! crate docs\n\npub fn a() {}\n";
        let resolved = resolve(source, "a").unwrap();
        let updated = apply(
            source,
            &resolved,
            "pub fn a() {}",
            &["mod math;".to_string()],
        )
        .unwrap();
        assert!(updated.starts_with("//! crate docs"));
        assert!(updated.contains("mod math;"));
        assert!(
            updated.find("//! crate docs").unwrap() < updated.find("mod math;").unwrap(),
            "{updated}"
        );
        assert_eq!(error_count(&updated).unwrap(), 0);
    }

    #[test]
    fn links_new_module_files_to_their_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();

        let flat = module_declaration(dir.path(), Path::new("src/math.rs")).unwrap();
        assert_eq!(flat.parent, PathBuf::from("src/lib.rs"));
        assert_eq!(flat.module, "math");

        std::fs::create_dir_all(dir.path().join("src/geometry")).unwrap();
        std::fs::write(dir.path().join("src/geometry/mod.rs"), "pub fn b() {}\n").unwrap();
        let nested = module_declaration(dir.path(), Path::new("src/geometry/shape.rs")).unwrap();
        assert_eq!(nested.parent, PathBuf::from("src/geometry/mod.rs"));
        assert_eq!(nested.module, "shape");

        let dir_mod = module_declaration(dir.path(), Path::new("src/geometry/mod.rs")).unwrap();
        assert_eq!(dir_mod.parent, PathBuf::from("src/lib.rs"));
        assert_eq!(dir_mod.module, "geometry");

        assert!(module_declaration(dir.path(), Path::new("src/lib.rs")).is_none());
        assert!(module_declaration(dir.path(), Path::new("src/main.rs")).is_none());
    }

    #[test]
    fn detects_and_inserts_module_declarations() {
        let source = "//! docs\n\npub fn a() {}\n";
        assert!(module_missing(source, "math"));
        let updated = add_module(source, "math").unwrap();
        assert!(updated.starts_with("//! docs"));
        assert!(updated.contains("mod math;"));
        assert!(!module_missing(&updated, "math"));
        assert_eq!(error_count(&updated).unwrap(), 0);

        let private = "mod math;\n";
        assert!(!module_missing(private, "math"));
        let public = "pub mod math;\n";
        assert!(!module_missing(public, "math"));
    }

    #[test]
    fn inserts_new_imports_once() {
        let resolved = resolve(SAMPLE, "add_one").unwrap();
        let new_code = "pub fn add_one(x: i32) -> i32 {\n    x + 2\n}";
        let updated = apply(
            SAMPLE,
            &resolved,
            new_code,
            &["use std::fmt::Debug;".to_string()],
        )
        .unwrap();
        assert_eq!(updated.matches("use std::fmt::Debug;").count(), 1);
        assert_eq!(error_count(&updated).unwrap(), 0);
    }

    #[test]
    fn rejects_ambiguous_short_names() {
        let source = "mod a { fn dup() {} }\nmod b { fn dup() {} }\n";
        let error = resolve(source, "dup").unwrap_err().to_string();
        assert!(error.contains("ambiguous"), "{error}");
    }

    #[test]
    fn rejects_missing_symbols() {
        let error = resolve(SAMPLE, "nope").unwrap_err().to_string();
        assert!(error.contains("not found"), "{error}");
    }
}
