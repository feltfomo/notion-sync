//! Map a file extension to a Notion code-block `language` enum value.
//! Unknown extensions fall back to "plain text" (a valid enum member).

use std::path::Path;

pub fn for_path(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    match ext.as_deref() {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("ts") => "typescript",
        Some("tsx") => "typescript",
        Some("js") | Some("mjs") | Some("cjs") => "javascript",
        Some("jsx") => "javascript",
        Some("go") => "go",
        Some("c") | Some("h") => "c",
        Some("cc") | Some("cpp") | Some("cxx") | Some("hpp") | Some("hxx") => "c++",
        Some("cs") => "c#",
        Some("java") => "java",
        Some("kt") | Some("kts") => "kotlin",
        Some("swift") => "swift",
        Some("rb") => "ruby",
        Some("php") => "php",
        Some("scala") => "scala",
        Some("hs") => "haskell",
        Some("lua") => "lua",
        Some("r") => "r",
        Some("dart") => "dart",
        Some("sh") | Some("bash") => "shell",
        Some("sql") => "sql",
        Some("html") | Some("htm") => "html",
        Some("css") => "css",
        Some("scss") | Some("sass") => "scss",
        Some("json") => "json",
        Some("yaml") | Some("yml") => "yaml",
        Some("toml") => "toml",
        Some("xml") => "xml",
        Some("md") | Some("markdown") => "markdown",
        Some("nix") => "nix",
        Some("dockerfile") => "docker",
        Some("makefile") | Some("mk") => "makefile",
        Some("graphql") | Some("gql") => "graphql",
        Some("tex") => "latex",
        Some("ml") | Some("mli") => "ocaml",
        Some("pl") | Some("pm") => "perl",
        Some("ps1") => "powershell",
        _ => match path.file_name().and_then(|n| n.to_str()) {
            Some("Dockerfile") => "docker",
            Some("Makefile") | Some("makefile") => "makefile",
            _ => "plain text",
        },
    }
}
