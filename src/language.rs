//! Map a file extension to a Notion code-block `language` enum value.
//! Unknown extensions fall back to "plain text" (a valid enum member).

use std::path::Path;

fn lang_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "rust",
        "py" => "python",
        "ts" => "typescript",
        "tsx" => "typescript",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascript",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" => "c++",
        "cs" => "c#",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "rb" => "ruby",
        "php" => "php",
        "scala" => "scala",
        "hs" => "haskell",
        "lua" => "lua",
        "r" => "r",
        "dart" => "dart",
        "sh" | "bash" => "shell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" | "sass" => "scss",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "nix" => "nix",
        "dockerfile" => "docker",
        "makefile" | "mk" => "makefile",
        "graphql" | "gql" => "graphql",
        "tex" => "latex",
        "ml" | "mli" => "ocaml",
        "pl" | "pm" => "perl",
        "ps1" => "powershell",
        _ => return None,
    })
}

pub fn for_path(path: &Path) -> &'static str {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        // Extensions are almost always lowercase, so match the borrowed &str first and
        // skip the to_ascii_lowercase allocation; only uppercase extensions pay the retry.
        if let Some(lang) = lang_for_ext(ext) {
            return lang;
        }
        if ext.bytes().any(|b| b.is_ascii_uppercase()) {
            if let Some(lang) = lang_for_ext(&ext.to_ascii_lowercase()) {
                return lang;
            }
        }
    }
    match path.file_name().and_then(|n| n.to_str()) {
        Some("Dockerfile") => "docker",
        Some("Makefile") | Some("makefile") => "makefile",
        _ => "plain text",
    }
}

#[cfg(test)]
mod tests {
    use super::for_path;
    use std::path::Path;

    fn lang(p: &str) -> &'static str {
        for_path(Path::new(p))
    }

    #[test]
    fn common_extensions_map() {
        assert_eq!(lang("src/main.rs"), "rust");
        assert_eq!(lang("app.py"), "python");
        assert_eq!(lang("a.tsx"), "typescript");
        assert_eq!(lang("style.scss"), "scss");
        assert_eq!(lang("q.gql"), "graphql");
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        // Raw-first matching must still fall back to a lowercase retry for uppercase
        // extensions.
        assert_eq!(lang("README.RS"), "rust");
        assert_eq!(lang("Main.Py"), "python");
        assert_eq!(lang("INDEX.HTML"), "html");
    }

    #[test]
    fn dockerfile_and_makefile_fall_back_to_filename() {
        assert_eq!(lang("Dockerfile"), "docker");
        assert_eq!(lang("Makefile"), "makefile");
        assert_eq!(lang("makefile"), "makefile");
        // Extension forms still resolve too.
        assert_eq!(lang("service.Dockerfile"), "docker");
        assert_eq!(lang("build.mk"), "makefile");
    }

    #[test]
    fn unknown_or_extensionless_is_plain_text() {
        assert_eq!(lang("data.bin"), "plain text");
        assert_eq!(lang("noext"), "plain text");
        assert_eq!(lang(".gitignore"), "plain text");
    }
}
