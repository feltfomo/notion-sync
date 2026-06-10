//! Map a file extension to a Notion code-block `language` enum value.
//! Unknown extensions fall back to "plain text" (a valid enum member).

use std::path::Path;

fn lang_for_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "rust",
        "py" | "pyw" | "pyi" => "python",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescript",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascript",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" => "c++",
        "cs" => "c#",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "rb" => "ruby",
        "php" => "php",
        "scala" | "sc" => "scala",
        "hs" => "haskell",
        "lua" => "lua",
        "r" => "r",
        "dart" => "dart",
        "sh" | "bash" | "zsh" => "shell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
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
        "ps1" | "psm1" | "psd1" => "powershell",
        "abap" => "abap",
        "agda" => "agda",
        "ino" => "arduino",
        "asm" | "s" => "assembly",
        "bas" => "basic",
        "clj" | "cljs" | "cljc" | "edn" => "clojure",
        "coffee" => "coffeescript",
        "dhall" => "dhall",
        "diff" | "patch" => "diff",
        "ex" | "exs" => "elixir",
        "elm" => "elm",
        "erl" | "hrl" => "erlang",
        "fs" | "fsi" | "fsx" => "f#",
        "f" | "for" | "f90" | "f95" | "f03" => "fortran",
        "feature" => "gherkin",
        "glsl" | "vert" | "frag" | "comp" => "glsl",
        "groovy" | "gradle" => "groovy",
        "hcl" | "tf" | "tfvars" => "hcl",
        "idr" => "idris",
        "jl" => "julia",
        "lisp" | "lsp" | "el" => "lisp",
        "ls" => "livescript",
        "ll" => "llvm ir",
        "nb" | "wl" | "wls" => "mathematica",
        "mm" => "objective-c",
        "mmd" => "mermaid",
        "pas" | "pp" => "pascal",
        "pro" => "prolog",
        "proto" => "protobuf",
        "purs" => "purescript",
        "rkt" => "racket",
        "re" | "rei" => "reason",
        "scm" | "ss" => "scheme",
        "sol" => "solidity",
        "st" => "smalltalk",
        "sv" | "svh" => "verilog",
        "vb" => "vb.net",
        "vhd" | "vhdl" => "vhdl",
        "wat" => "webassembly",
        // Shared extensions: follow GitHub Linguist's default instead of guessing per file.
        "m" => "objective-c",
        "v" => "verilog",
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

    #[test]
    fn extended_languages_map() {
        assert_eq!(lang("app.ex"), "elixir");
        assert_eq!(lang("core.clj"), "clojure");
        assert_eq!(lang("node.erl"), "erlang");
        assert_eq!(lang("Lib.fs"), "f#");
        assert_eq!(lang("shader.frag"), "glsl");
        assert_eq!(lang("build.gradle"), "groovy");
        assert_eq!(lang("main.jl"), "julia");
        assert_eq!(lang("token.sol"), "solidity");
        assert_eq!(lang("schema.proto"), "protobuf");
        assert_eq!(lang("infra.tf"), "hcl");
    }

    #[test]
    fn shared_extensions_follow_linguist_defaults() {
        // These extensions are claimed by more than one language; we commit to Linguist's
        // default rather than sniff file contents.
        assert_eq!(lang("legacy.m"), "objective-c");
        assert_eq!(lang("cpu.v"), "verilog");
        assert_eq!(lang("solve.pl"), "perl");
        assert_eq!(lang("solve.pro"), "prolog");
    }
}
