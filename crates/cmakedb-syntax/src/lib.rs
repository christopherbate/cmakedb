//! L1 syntax layer (design §3.1).
//!
//! Parses `.cmake` / `CMakeLists.txt` files into lossless ASTs via
//! tree-sitter-cmake, assigns position-stable node IDs, and extracts the
//! information the semantic database joins against: command invocations,
//! argument spans, and variable references.

pub mod varref;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Kind of a CMake command argument, per the real grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    Quoted,
    Unquoted,
    Bracket,
}

#[derive(Debug, Clone)]
pub struct Argument {
    pub kind: ArgKind,
    /// Raw source text including quotes/brackets.
    pub text: String,
    /// Text with quotes/brackets stripped (still unexpanded).
    pub value: String,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// A command invocation (`normal_command` or a block-structure keyword like
/// `if`/`foreach`/`function`, which the grammar models separately).
#[derive(Debug, Clone)]
pub struct CommandInvocation {
    /// Index into `ParsedFile::nodes` of the invocation node.
    pub node: usize,
    pub name: String,
    pub name_lower: String,
    /// 1-based line of the command name.
    pub line: usize,
    pub col: usize,
    pub args: Vec<Argument>,
    /// Statically-resolvable variable names referenced in the raw argument
    /// text (`${X}`, nested inner refs, `$CACHE{X}`). `$ENV{X}` is excluded.
    pub var_refs: Vec<String>,
    /// Statically-named environment references (`$ENV{X}`) in the raw
    /// argument text. Kept separate from `var_refs`: these read the
    /// process environment, not CMake variables.
    pub env_refs: Vec<String>,
}

/// One AST node, flattened. `id` is the index in `ParsedFile::nodes`;
/// `parent` likewise. Node identity across recordings is
/// `(content_hash, byte_start, kind)` per design §3.1.
#[derive(Debug, Clone)]
pub struct AstNode {
    pub kind: &'static str,
    pub byte_start: usize,
    pub byte_end: usize,
    /// 1-based.
    pub line: usize,
    /// 1-based.
    pub col: usize,
    pub parent: Option<usize>,
}

#[derive(Debug)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub content: String,
    /// Hex SHA-256 of content.
    pub content_hash: String,
    pub nodes: Vec<AstNode>,
    pub commands: Vec<CommandInvocation>,
    /// True if tree-sitter reported syntax errors (parse is still usable).
    pub has_errors: bool,
}

impl ParsedFile {
    /// All command invocations on a given 1-based line, in source order.
    pub fn commands_at_line(&self, line: usize) -> impl Iterator<Item = &CommandInvocation> {
        self.commands.iter().filter(move |c| c.line == line)
    }

    /// Whether the node (by index) has an ancestor of one of the given kinds.
    pub fn has_ancestor_of_kind(&self, node: usize, kinds: &[&str]) -> bool {
        let mut cur = self.nodes[node].parent;
        while let Some(i) = cur {
            if kinds.contains(&self.nodes[i].kind) {
                return true;
            }
            cur = self.nodes[i].parent;
        }
        false
    }
}

pub fn hash_content(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("{:x}", h.finalize())
}

pub fn parse_file(path: &Path) -> Result<ParsedFile> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_source(path.to_path_buf(), content)
}

pub fn parse_source(path: PathBuf, content: String) -> Result<ParsedFile> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_cmake::LANGUAGE.into())
        .context("loading tree-sitter-cmake grammar")?;
    let tree = parser
        .parse(&content, None)
        .context("tree-sitter parse returned no tree")?;

    let mut nodes: Vec<AstNode> = Vec::new();
    let mut commands: Vec<CommandInvocation> = Vec::new();
    let has_errors = tree.root_node().has_error();

    // Flatten the tree depth-first, keeping parent links.
    let mut stack: Vec<(tree_sitter::Node, Option<usize>)> = vec![(tree.root_node(), None)];
    while let Some((node, parent)) = stack.pop() {
        let idx = nodes.len();
        nodes.push(AstNode {
            kind: node.kind(),
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
            line: node.start_position().row + 1,
            col: node.start_position().column + 1,
            parent,
        });

        if let Some(cmd) = extract_command(&node, idx, &content) {
            commands.push(cmd);
        }

        // Push children in reverse so they are visited in source order.
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push((child, Some(idx)));
        }
    }

    commands.sort_by_key(|c| {
        let n = &nodes[c.node];
        (n.byte_start, n.byte_end)
    });

    Ok(ParsedFile {
        content_hash: hash_content(&content),
        path,
        content,
        nodes,
        commands,
        has_errors,
    })
}

/// Grammar note (tree-sitter-cmake): plain commands are `normal_command`
/// with an `identifier` child; control-flow keywords get dedicated kinds
/// (`if_command`, `elseif_command`, `else_command`, `endif_command`,
/// `foreach_command`, `while_command`, `function_command`, `macro_command`,
/// `block_command`, and matching `end*_command`), each containing an
/// `argument_list`.
fn extract_command(
    node: &tree_sitter::Node,
    node_idx: usize,
    content: &str,
) -> Option<CommandInvocation> {
    let kind = node.kind();
    if !kind.ends_with("_command") {
        return None;
    }
    let text = |n: &tree_sitter::Node| content[n.byte_range()].to_string();

    // Command name: the `identifier` child for normal_command; for keyword
    // commands the grammar stores the keyword as an anonymous first token.
    let mut name = None;
    let mut args = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" if name.is_none() => name = Some(text(&child)),
            "argument_list" => {
                let mut ac = child.walk();
                for arg in child.children(&mut ac) {
                    if arg.kind() == "argument" {
                        // argument wraps quoted/unquoted/bracket_argument
                        if let Some(inner) = arg.named_child(0) {
                            args.push(make_argument(&inner, content));
                        }
                    } else {
                        match arg.kind() {
                            "quoted_argument" | "unquoted_argument" | "bracket_argument" => {
                                args.push(make_argument(&arg, content))
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let name = name.unwrap_or_else(|| {
        // Keyword commands: derive from node kind, e.g. "if_command" -> "if".
        kind.trim_end_matches("_command").to_string()
    });

    let var_refs = {
        let mut v: Vec<String> = args
            .iter()
            .flat_map(|a| varref::static_var_refs(&a.text))
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let env_refs = {
        let mut v: Vec<String> = args
            .iter()
            .flat_map(|a| varref::env_var_refs(&a.text))
            .collect();
        v.sort();
        v.dedup();
        v
    };

    Some(CommandInvocation {
        node: node_idx,
        name_lower: name.to_ascii_lowercase(),
        name,
        line: node.start_position().row + 1,
        col: node.start_position().column + 1,
        args,
        var_refs,
        env_refs,
    })
}

fn make_argument(node: &tree_sitter::Node, content: &str) -> Argument {
    let raw = content[node.byte_range()].to_string();
    let (kind, value) = match node.kind() {
        "quoted_argument" => (
            ArgKind::Quoted,
            raw.trim_start_matches('"')
                .trim_end_matches('"')
                .to_string(),
        ),
        "bracket_argument" => (ArgKind::Bracket, strip_bracket(&raw)),
        _ => (ArgKind::Unquoted, raw.clone()),
    };
    Argument {
        kind,
        value,
        text: raw,
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
    }
}

fn strip_bracket(raw: &str) -> String {
    // [=*[ ... ]=*]
    if let Some(open_end) = raw.find('[') {
        if let Some(second) = raw[open_end + 1..].find('[') {
            let eq = second; // number of '=' between the two '['
            let start = open_end + 1 + second + 1;
            let end = raw.len().saturating_sub(eq + 2);
            if start <= end {
                return raw[start..end].to_string();
            }
        }
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> ParsedFile {
        parse_source(PathBuf::from("test.cmake"), src.to_string()).unwrap()
    }

    #[test]
    fn parses_normal_command() {
        let p = parse("set(FOO bar)\n");
        let cmds: Vec<_> = p.commands.iter().map(|c| c.name_lower.as_str()).collect();
        assert!(cmds.contains(&"set"), "commands: {cmds:?}");
        let set = p.commands.iter().find(|c| c.name_lower == "set").unwrap();
        assert_eq!(set.args.len(), 2);
        assert_eq!(set.args[0].value, "FOO");
        assert_eq!(set.line, 1);
    }

    #[test]
    fn parses_keyword_commands_and_var_refs() {
        let p = parse("if(${ENABLE_X})\n  set(Y \"${PREFIX}_lib\")\nendif()\n");
        let ifc = p.commands.iter().find(|c| c.name_lower == "if").unwrap();
        assert_eq!(ifc.var_refs, vec!["ENABLE_X".to_string()]);
        let set = p.commands.iter().find(|c| c.name_lower == "set").unwrap();
        assert_eq!(set.var_refs, vec!["PREFIX".to_string()]);
    }

    #[test]
    fn multiple_commands_per_line() {
        let p = parse("set(A 1)\nset(B 2) set(C 3)\n");
        assert_eq!(p.commands_at_line(2).count(), 2);
    }

    #[test]
    fn loop_ancestry() {
        let p = parse("foreach(i 1 2)\n  set(V ${i})\nendforeach()\n");
        let set = p.commands.iter().find(|c| c.name_lower == "set").unwrap();
        assert!(p.has_ancestor_of_kind(
            set.node,
            &[
                "foreach_loop",
                "foreach_command",
                "while_loop",
                "while_command"
            ]
        ));
    }

    #[test]
    fn bracket_argument() {
        let p = parse("set(DOC [=[hello ${NOT_A_REF} world]=])\n");
        let set = p.commands.iter().find(|c| c.name_lower == "set").unwrap();
        assert_eq!(set.args[1].value, "hello ${NOT_A_REF} world");
        // bracket args suppress expansion -> no var refs
        assert!(set.var_refs.is_empty());
    }
}

/// Lossless reprint (design §3.1, §6.3): reconstruct the exact source text
/// from the flattened AST's leaf spans plus the gaps between them. This is
/// the property the patch engine depends on — node byte ranges must tile
/// the file coherently so byte-anchored edits leave untouched bytes
/// untouched. Returns an error if spans overlap or run backwards.
pub fn reprint(file: &ParsedFile) -> Result<String> {
    // Leaves = nodes no other node claims as parent.
    let mut has_child = vec![false; file.nodes.len()];
    for n in &file.nodes {
        if let Some(p) = n.parent {
            has_child[p] = true;
        }
    }
    let mut leaves: Vec<&AstNode> = file
        .nodes
        .iter()
        .enumerate()
        .filter(|(i, _)| !has_child[*i])
        .map(|(_, n)| n)
        .collect();
    leaves.sort_by_key(|n| (n.byte_start, n.byte_end));

    let mut out = String::with_capacity(file.content.len());
    let mut cursor = 0usize;
    for leaf in leaves {
        if leaf.byte_start < cursor {
            anyhow::bail!(
                "overlapping AST spans at byte {} (kind {})",
                leaf.byte_start,
                leaf.kind
            );
        }
        out.push_str(&file.content[cursor..leaf.byte_start]); // inter-token gap
        out.push_str(&file.content[leaf.byte_start..leaf.byte_end]);
        cursor = leaf.byte_end;
    }
    out.push_str(&file.content[cursor..]);
    Ok(out)
}

#[cfg(test)]
mod reprint_tests {
    use super::*;

    fn roundtrip(src: &str) {
        let p = parse_source(PathBuf::from("t.cmake"), src.to_string()).unwrap();
        assert_eq!(reprint(&p).unwrap(), src, "reprint must be byte-identical");
    }

    #[test]
    fn roundtrips_preserve_bytes() {
        roundtrip("");
        roundtrip("# only a comment\n");
        roundtrip("set(A 1)\n");
        roundtrip("set(A 1) # trailing comment\nset(B \"x y\")\n");
        roundtrip("if(FOO)\n  set(X ${Y})   # weird   spacing\nendif()\n");
        roundtrip("set(DOC [=[bracket ${NOT} \n multi]=])\n");
        roundtrip("set(ML \"multi\nline\")\nfunction(f)\nendfunction()\n");
        roundtrip("set(G \"$<$<CONFIG:Debug>:d>\")\r\n"); // CRLF survives
        roundtrip("cmd(a\tb)\n\n\n# gap\n");
        // error-tolerant parse still reprints losslessly
        roundtrip("set(A 1) set(B 2)\nif(\n");
    }

    #[test]
    fn roundtrips_fixture_corpus() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        let mut checked = 0;
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().map(|x| x == "cmake").unwrap_or(false)
                    || p.file_name()
                        .map(|n| n == "CMakeLists.txt")
                        .unwrap_or(false)
                {
                    let src = std::fs::read_to_string(&p).unwrap();
                    let parsed = parse_source(p.clone(), src.clone()).unwrap();
                    assert_eq!(reprint(&parsed).unwrap(), src, "{}", p.display());
                    checked += 1;
                }
            }
        }
        assert!(
            checked >= 8,
            "fixture corpus should be covered, got {checked}"
        );
    }
}
