//! Rules that neither the compiler nor clippy checks, read from the sources with `syn`.
//!
//! - Bare numbers in UI code. Clippy has no lint for them, and a number scaling a theme token
//!   (`theme.label_width() * 0.75`) gets past the sizes banned in `clippy.toml`.
//! - A target OS named outside the modules that own what differs per OS (`docs/DESIGN.md`,
//!   "Goals and non-goals (v1)"). The compiler sees only its own OS's side of a `cfg`; the
//!   source shows every side on every OS. The core crate is read from here, where `syn` already
//!   is.
//!
//! A test binary of its own costs little here: it uses neither gpui nor Ely, so it links neither.

use std::{
    fs,
    path::{Path, PathBuf},
};

use proc_macro2::{Delimiter, Ident, Span, TokenStream, TokenTree};
use syn::{
    AttrStyle, Attribute, Lit, LitFloat, LitInt, Meta, UseName, UseRename, UseTree,
    parse::Parser,
    visit::{self, Visit},
};

/// Sorted, so a failure lists its hits in the same order on every OS.
fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{} is unreadable: {error}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files.extend(sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels below the workspace root")
}

/// Joined with `/` on every OS, so a failure reads the same everywhere and a file compares
/// equal to its entry in [`OS_HOMES`] on Windows too.
fn shown(path: &Path) -> String {
    path.strip_prefix(workspace())
        .expect("under the workspace")
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} is unreadable: {error}", path.display()))
}

/// Zero and one need no name, and a `const` or `static` item is where a number gets its name.
struct Numbers {
    file: String,
    hits: Vec<String>,
}

impl Numbers {
    fn hit(&mut self, span: Span, text: String) {
        self.hits
            .push(format!("{}:{}: {text}", self.file, span.start().line));
    }

    fn int(&mut self, lit: &LitInt) {
        if !matches!(lit.base10_parse::<u128>(), Ok(0 | 1)) {
            self.hit(lit.span(), lit.to_string());
        }
    }

    fn float(&mut self, lit: &LitFloat) {
        if !matches!(lit.base10_parse::<f64>(), Ok(value) if value == 0.0 || value == 1.0) {
            self.hit(lit.span(), lit.to_string());
        }
    }

    /// Macro arguments are tokens, not syntax. Those that read as Rust statements are checked
    /// as such, so a `const` among them still names its number. The rest go token by token: a
    /// literal after a lone `.` is a tuple index; after `..` it ends a range.
    fn tokens(&mut self, tokens: TokenStream) {
        if let Ok(statements) = syn::Block::parse_within.parse2(tokens.clone()) {
            for statement in &statements {
                self.visit_stmt(statement);
            }
            return;
        }
        let mut dots = 0;
        for token in tokens {
            let tuple_index = dots == 1;
            match &token {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Literal(literal) if !tuple_index => match Lit::new(literal.clone()) {
                    Lit::Int(lit) => self.int(&lit),
                    Lit::Float(lit) => self.float(&lit),
                    _ => {}
                },
                _ => {}
            }
            dots = match &token {
                TokenTree::Punct(punct) if punct.as_char() == '.' => dots + 1,
                _ => 0,
            };
        }
    }
}

/// The bare numbers in `source`, each as `file:line: number`.
fn bare_numbers(file: &str, source: &str) -> Vec<String> {
    let ast =
        syn::parse_file(source).unwrap_or_else(|error| panic!("{file} does not parse: {error}"));
    let mut numbers = Numbers {
        file: file.to_owned(),
        hits: Vec::new(),
    };
    numbers.visit_file(&ast);
    numbers.hits
}

impl<'ast> Visit<'ast> for Numbers {
    fn visit_item_const(&mut self, _: &'ast syn::ItemConst) {}

    fn visit_item_static(&mut self, _: &'ast syn::ItemStatic) {}

    fn visit_impl_item_const(&mut self, _: &'ast syn::ImplItemConst) {}

    fn visit_trait_item_const(&mut self, _: &'ast syn::TraitItemConst) {}

    fn visit_lit_int(&mut self, lit: &'ast LitInt) {
        self.int(lit);
    }

    fn visit_lit_float(&mut self, lit: &'ast LitFloat) {
        self.float(lit);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.tokens(mac.tokens.clone());
        visit::visit_macro(self, mac);
    }
}

#[test]
fn ui_code_names_its_numbers() {
    let mut hits = Vec::new();
    for path in sources(&workspace().join("crates/app/src/ui")) {
        hits.extend(bare_numbers(&shown(&path), &read(&path)));
    }
    assert!(
        hits.is_empty(),
        "UI code uses bare numbers. Name each one as a `const` that says what it means, or take \
         it from the theme (`cx.theme()`); a size the theme has no token for belongs in \
         `ui::metrics`.\n{}",
        hits.join("\n")
    );
}

fn found(body: &str) -> Vec<String> {
    bare_numbers("probe.rs", &format!("fn probe() {{ {body} }}"))
}

#[test]
fn in_a_macro_the_ends_of_a_range_are_numbers_and_a_tuple_index_is_not() {
    assert_eq!(found("vec![0..24];"), ["probe.rs:1: 24"]);
    assert_eq!(found("vec![0..=24];"), ["probe.rs:1: 24"]);
    assert_eq!(found("vec![tuple.2];"), [""; 0]);
    assert_eq!(found("vec![nested.2.3];"), [""; 0]);
    // Arguments that are not statements, so they are read token by token.
    assert_eq!(found(r#"format!("{}", 0..24);"#), ["probe.rs:1: 24"]);
    assert_eq!(found(r#"format!("{} {}", tuple.2, nested.2.3);"#), [""; 0]);
}

#[test]
fn in_a_macro_a_const_still_names_its_number() {
    assert_eq!(found("vec![{ const COUNT: usize = 24; COUNT }];"), [""; 0]);
    assert_eq!(found("vec![{ 24 }];"), ["probe.rs:1: 24"]);
    assert_eq!(
        found(r#"format!("{}", { const COUNT: usize = 24; COUNT });"#),
        [""; 0]
    );
    assert_eq!(found(r#"format!("{}", { 24 });"#), ["probe.rs:1: 24"]);
}

/// Only these files may name a target OS.
const OS_HOMES: [&str; 5] = [
    // The app's per-OS module.
    "crates/app/src/platform.rs",
    // The `Host` abstraction: where commands run.
    "crates/core/src/host.rs",
    // Where data lives: it makes files owner-only, which only unix can express.
    "crates/core/src/paths.rs",
    // Which asset a release ships for this platform.
    "crates/core/src/update.rs",
    // Downloading, extracting and swapping an update differs per OS.
    "crates/core/src/self_update.rs",
];

/// Identifiers, not text: `windows_subsystem`, `feature = "windows"` and a "Windows" in a
/// string name no target OS.
const OS_WORDS: [&str; 6] = [
    "windows",
    "unix",
    "target_os",
    "target_family",
    "target_vendor",
    "target_env",
];

/// What `std::env::consts` tells the OS apart by at run time (`ARCH` does not).
const OS_CONSTS: [&str; 2] = ["OS", "FAMILY"];

fn os_word(tokens: impl IntoIterator<Item = TokenTree>) -> Option<Span> {
    tokens.into_iter().find_map(|token| match token {
        TokenTree::Ident(ident) if OS_WORDS.iter().any(|word| ident == word) => Some(ident.span()),
        TokenTree::Group(group) => os_word(group.stream()),
        _ => None,
    })
}

fn is_os_const(ident: &Ident) -> bool {
    OS_CONSTS.iter().any(|name| ident == name)
}

/// Exactly `#[cfg(test)]`: a gate that skips a test cannot make the product differ per OS.
fn is_test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && attr.parse_args::<Ident>().is_ok_and(|arg| arg == "test")
    })
}

/// Where `mod name;` in `file` finds `name`'s file: beside a `mod.rs` or a crate root, else in
/// the directory named after the file.
fn module_dir(file: &str) -> String {
    let stem = file.strip_suffix(".rs").unwrap_or(file);
    match stem.rsplit_once('/') {
        Some((dir, "mod")) => dir.to_owned(),
        Some((dir, "lib" | "main")) if dir.ends_with("/src") => dir.to_owned(),
        _ => stem.to_owned(),
    }
}

/// After `consts::` in tokens: a name read at run time, or a `use` of one (`{ARCH, OS}`, `*`).
fn os_const(token: &TokenTree) -> Option<String> {
    match token {
        TokenTree::Ident(ident) if is_os_const(ident) => Some(ident.to_string()),
        TokenTree::Punct(punct) if punct.as_char() == '*' => Some("*".to_owned()),
        TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => group
            .stream()
            .into_iter()
            .find_map(|token| os_const(&token)),
        _ => None,
    }
}

/// The first name `tree` imports from a `consts` module that tells the OS apart.
fn imported_os(tree: &UseTree, from_consts: bool) -> Option<(Span, String)> {
    match tree {
        UseTree::Path(path) => imported_os(&path.tree, path.ident == "consts"),
        UseTree::Name(UseName { ident }) | UseTree::Rename(UseRename { ident, .. })
            if from_consts && is_os_const(ident) =>
        {
            Some((ident.span(), ident.to_string()))
        }
        UseTree::Glob(glob) if from_consts => Some((glob.star_token.spans[0], "*".to_owned())),
        UseTree::Group(group) => group
            .items
            .iter()
            .find_map(|item| imported_os(item, from_consts)),
        _ => None,
    }
}

/// One hit per `cfg`, `cfg_attr`, `cfg!`, `cfg_select!` arm or `consts` path, however many OS
/// words it holds.
struct OsNames {
    file: String,
    /// Where a `mod name;` met here finds `name`'s file.
    dir: String,
    /// The files of the `#[cfg(test)] mod name;` met here, without `.rs`: they are test modules
    /// too, though the walk reaches them as files of their own.
    test_modules: Vec<String>,
    hits: Vec<String>,
}

impl OsNames {
    fn hit(&mut self, span: Span, text: String) {
        self.hits
            .push(format!("{}:{}: {text}", self.file, span.start().line));
    }

    /// `name(args)` or `name!(args)`, inside macro arguments or as a macro itself.
    fn call(&mut self, name: &Ident, bang: bool, args: TokenStream) {
        if name == "cfg" || name == "cfg_attr" {
            if os_word(args).is_some() {
                let bang = if bang { "!" } else { "" };
                self.hit(name.span(), format!("{name}{bang}(..)"));
            }
        } else if name == "cfg_select" && bang {
            self.arms(args);
        } else {
            self.tokens(args);
        }
    }

    /// Macro arguments are tokens, not syntax, so a `cfg!` nested in `assert!` or a `#[cfg]`
    /// inside `cfg_if!` is found here, and so is a `consts::OS` in `format!`.
    fn tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        let mut rest = tokens.as_slice();
        while let [token, tail @ ..] = rest {
            rest = tail;
            match token {
                TokenTree::Ident(name) => {
                    if name == "consts"
                        && let [TokenTree::Punct(first), TokenTree::Punct(second), next, ..] = tail
                        && first.as_char() == ':'
                        && second.as_char() == ':'
                        && let Some(what) = os_const(next)
                    {
                        self.hit(name.span(), format!("consts::{what}"));
                    }
                    let (bang, after) = match tail {
                        [TokenTree::Punct(punct), after @ ..] if punct.as_char() == '!' => {
                            (true, after)
                        }
                        _ => (false, tail),
                    };
                    if let [TokenTree::Group(args), after @ ..] = after {
                        self.call(name, bang, args.stream());
                        rest = after;
                    }
                }
                TokenTree::Group(group) => self.tokens(group.stream()),
                _ => {}
            }
        }
    }

    /// A `cfg_select!` arm opens with a bare predicate (`windows => { .. }`), so an OS word
    /// outside the brace-delimited arm bodies is one.
    fn arms(&mut self, tokens: TokenStream) {
        let mut predicate = None;
        for token in tokens {
            match token {
                TokenTree::Group(body) if body.delimiter() == Delimiter::Brace => {
                    if let Some(span) = predicate.take() {
                        self.hit(span, "cfg_select! arm".to_owned());
                    }
                    self.tokens(body.stream());
                }
                token => predicate = predicate.or_else(|| os_word([token])),
            }
        }
        if let Some(span) = predicate {
            self.hit(span, "cfg_select! arm".to_owned());
        }
    }
}

impl<'ast> Visit<'ast> for OsNames {
    fn visit_file(&mut self, file: &'ast syn::File) {
        if !is_test_only(&file.attrs) {
            visit::visit_file(self, file);
        }
    }

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        let dir = format!("{}/{}", self.dir, module.ident);
        if is_test_only(&module.attrs) {
            if module.content.is_none() {
                self.test_modules.push(dir);
            }
            return;
        }
        let outer = std::mem::replace(&mut self.dir, dir);
        visit::visit_item_mod(self, module);
        self.dir = outer;
    }

    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        if let Some(name) = attr.path().get_ident()
            && (name == "cfg" || name == "cfg_attr")
            && let Meta::List(list) = &attr.meta
            && os_word(list.tokens.clone()).is_some()
        {
            let bang = match attr.style {
                AttrStyle::Inner(_) => "!",
                AttrStyle::Outer => "",
            };
            self.hit(attr.pound_token.spans[0], format!("#{bang}[{name}(..)]"));
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if let Some(last) = mac.path.segments.last() {
            self.call(&last.ident, true, mac.tokens.clone());
        }
        visit::visit_macro(self, mac);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let pairs = path.segments.iter().zip(path.segments.iter().skip(1));
        for (module, name) in pairs {
            if module.ident == "consts" && is_os_const(&name.ident) {
                self.hit(module.ident.span(), format!("consts::{}", name.ident));
            }
        }
        visit::visit_path(self, path);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if let Some((span, name)) = imported_os(&item.tree, false) {
            self.hit(span, format!("consts::{name}"));
        }
        visit::visit_item_use(self, item);
    }
}

/// The places in `files` (path, source) that name a target OS, each as `file:line: what`.
fn os_names(files: &[(String, String)]) -> Vec<String> {
    let scans: Vec<OsNames> = files
        .iter()
        .map(|(file, source)| {
            let ast = syn::parse_file(source)
                .unwrap_or_else(|error| panic!("{file} does not parse: {error}"));
            let mut names = OsNames {
                file: file.clone(),
                dir: module_dir(file),
                test_modules: Vec::new(),
                hits: Vec::new(),
            };
            names.visit_file(&ast);
            names
        })
        .collect();
    let test_modules: Vec<&String> = scans.iter().flat_map(|scan| &scan.test_modules).collect();
    scans
        .iter()
        .filter(|scan| {
            !test_modules.iter().any(|module| {
                scan.file == format!("{module}.rs") || scan.file.starts_with(&format!("{module}/"))
            })
        })
        .flat_map(|scan| scan.hits.clone())
        .collect()
}

#[test]
fn only_the_platform_modules_name_a_target_os() {
    let mut files = Vec::new();
    for dir in ["crates/app/src", "crates/core/src"] {
        for path in sources(&workspace().join(dir)) {
            files.push((shown(&path), read(&path)));
        }
    }
    let hits: Vec<String> = os_names(&files)
        .into_iter()
        .filter(|hit| {
            !OS_HOMES
                .iter()
                .any(|home| hit.starts_with(&format!("{home}:")))
        })
        .collect();
    assert!(
        hits.is_empty(),
        "Code outside the modules that own what differs per OS names a target OS. Move what \
         differs into `crates/app/src/platform.rs` (app) or behind `Host` in \
         `crates/core/src/host.rs` (core), and call that from here.\n{}",
        hits.join("\n")
    );
}

fn os_found(source: &str) -> Vec<String> {
    os_names(&[("probe.rs".to_owned(), source.to_owned())])
}

#[test]
fn a_cfg_that_names_a_target_os_is_found_once_however_deep() {
    assert_eq!(
        os_found("#[cfg(unix)] fn f() {}"),
        ["probe.rs:1: #[cfg(..)]"]
    );
    assert_eq!(os_found("#![cfg(windows)]"), ["probe.rs:1: #![cfg(..)]"]);
    assert_eq!(
        os_found(r#"#[cfg(not(any(target_os = "linux", unix)))] fn f() {}"#),
        ["probe.rs:1: #[cfg(..)]"]
    );
    assert_eq!(
        os_found("#[cfg_attr(windows, allow(dead_code))] fn f() {}"),
        ["probe.rs:1: #[cfg_attr(..)]"]
    );
    for word in [
        r#"target_family = "wasm""#,
        r#"target_vendor = "apple""#,
        r#"target_env = "musl""#,
    ] {
        assert_eq!(
            os_found(&format!("#[cfg({word})] fn f() {{}}")),
            ["probe.rs:1: #[cfg(..)]"]
        );
    }
    // On a block, not an item, and reported where it starts.
    let block = "fn f() {
    #[cfg(
        unix
    )]
    {}
}";
    assert_eq!(os_found(block), ["probe.rs:2: #[cfg(..)]"]);
    assert_eq!(
        os_found("fn f() -> bool { cfg!(windows) }"),
        ["probe.rs:1: cfg!(..)"]
    );
}

#[test]
fn a_cfg_inside_another_macro_is_found() {
    assert_eq!(
        os_found("fn f() { assert!(cfg!(windows)); }"),
        ["probe.rs:1: cfg!(..)"]
    );
    assert_eq!(
        os_found("fn f() { vec![cfg!(unix)]; }"),
        ["probe.rs:1: cfg!(..)"]
    );
    assert_eq!(
        os_found(r#"fn f() { format!("{}", cfg!(target_os = "macos")); }"#),
        ["probe.rs:1: cfg!(..)"]
    );
    assert_eq!(
        os_found("cfg_if::cfg_if! { if #[cfg(unix)] { fn f() {} } else { fn f() {} } }"),
        ["probe.rs:1: cfg(..)"]
    );
}

#[test]
fn each_cfg_select_arm_that_names_a_target_os_is_found() {
    let source = r#"cfg_select! {
    windows => { fn f() {} }
    any(unix, target_os = "wasi") => { fn f() {} }
    debug_assertions => { fn f() {} }
    _ => { fn f() {} }
}"#;
    assert_eq!(
        os_found(source),
        ["probe.rs:2: cfg_select! arm", "probe.rs:3: cfg_select! arm"]
    );
    assert_eq!(
        os_found("cfg_select! { _ => { fn f() -> bool { cfg!(unix) } } }"),
        ["probe.rs:1: cfg!(..)"]
    );
}

#[test]
fn reading_the_os_at_run_time_is_found() {
    assert_eq!(
        os_found(r#"fn f() { if std::env::consts::OS == "windows" {} }"#),
        ["probe.rs:1: consts::OS"]
    );
    assert_eq!(
        os_found("use std::env::consts::{ARCH, OS};"),
        ["probe.rs:1: consts::OS"]
    );
    assert_eq!(
        os_found("use std::env::{consts::FAMILY as Family, var};"),
        ["probe.rs:1: consts::FAMILY"]
    );
    assert_eq!(
        os_found("use std::env::consts::*;"),
        ["probe.rs:1: consts::*"]
    );
    assert_eq!(
        os_found(r#"fn f() { println!("{}", std::env::consts::OS); }"#),
        ["probe.rs:1: consts::OS"]
    );
}

#[test]
fn gates_that_do_not_tell_the_os_apart_are_not_found() {
    for source in [
        "#[cfg(test)] fn f() {}",
        r#"#[cfg(feature = "http")] fn f() {}"#,
        "fn f() -> bool { cfg!(debug_assertions) }",
        "fn f() { assert!(cfg!(debug_assertions)); }",
        r#"#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]"#,
        r#"#[test] #[ignore = "draws the Windows chrome"] fn f() {}"#,
        r#"fn f() -> &'static str { "windows" }"#,
        r#"#[cfg(feature = "windows")] fn f() {}"#,
        "fn f() -> &'static str { std::env::consts::ARCH }",
        "use std::env::consts::ARCH;",
    ] {
        assert_eq!(os_found(source), [""; 0], "{source}");
    }
}

#[test]
fn test_modules_may_name_a_target_os() {
    assert_eq!(
        os_found("#[cfg(test)] mod tests { #[cfg(unix)] #[test] fn f() {} }"),
        [""; 0]
    );
    assert_eq!(os_found("#![cfg(test)] #[cfg(unix)] fn f() {}"), [""; 0]);
    assert_eq!(
        os_found("#[cfg(test)] mod tests {} #[cfg(unix)] fn f() {}"),
        ["probe.rs:1: #[cfg(..)]"]
    );
    // A test module whose body is a file of its own.
    let gated = "#[cfg(unix)] fn f() {}";
    let file = |path: &str, source: &str| (path.to_owned(), source.to_owned());
    assert_eq!(
        os_names(&[
            file("crates/x/src/lib.rs", "#[cfg(test)] mod tests; mod a;"),
            file("crates/x/src/tests.rs", gated),
            file("crates/x/src/a.rs", "mod b { #[cfg(test)] mod tests; }"),
            file("crates/x/src/a/b/tests.rs", gated),
            file("crates/x/src/a/b/tests/deep.rs", gated),
            file("crates/x/src/c.rs", gated),
        ]),
        ["crates/x/src/c.rs:1: #[cfg(..)]"]
    );
}
