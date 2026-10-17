//! Persisted records are BinSchema-generated only (`CLAUDE.md`). A hand-rolled
//! encoding nearly always lays out integers itself, so this test fails on any
//! byte-order conversion (`to_be_bytes`, `from_le_bytes`, ...) or `byteorder` use
//! in the workspace's non-test code. Generated code, `tests/` directories, and
//! `#[cfg(test)]` items are skipped. Uses that persist nothing are listed in
//! [`NOT_PERSISTED`] with the reason.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use syn::visit::{self, Visit};

const CONVERSIONS: &[&str] = &[
    "to_be_bytes",
    "to_le_bytes",
    "to_ne_bytes",
    "from_be_bytes",
    "from_le_bytes",
    "from_ne_bytes",
];

/// Crates that persist nothing of Wheat's: the benchmark harness only builds
/// synthetic input, and the trace crate writes developer trace files.
const SKIPPED_CRATES: &[&str] = &["wheat-bench", "wheat-trace"];

/// `(file relative to crates/, enclosing function, why nothing is persisted)`.
const NOT_PERSISTED: &[(&str, &str, &str)] = &[(
    "wheat-core/src/guarded/matcher.rs",
    "reverse_part_matches",
    "compares words of two in-memory buffers",
)];

#[test]
fn no_hand_rolled_encodings_outside_generated_code() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in fs::read_dir(&crates).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if SKIPPED_CRATES.contains(&name.as_str()) {
            continue;
        }
        collect_sources(&entry.path().join("src"), &mut files);
    }
    assert!(files.len() > 100, "found only {} source files", files.len());

    let mut findings = Vec::new();
    for path in &files {
        let relative = path
            .strip_prefix(&crates)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let source = fs::read_to_string(path).unwrap();
        let file = syn::parse_file(&source).unwrap_or_else(|e| panic!("{relative}: {e}"));
        let mut finder = Finder {
            file: &relative,
            functions: Vec::new(),
            findings: &mut findings,
        };
        finder.visit_file(&file);
    }

    let mut report = String::new();
    for finding in &findings {
        let excused = NOT_PERSISTED
            .iter()
            .any(|(file, function, _)| *file == finding.file && *function == finding.function);
        if !excused {
            writeln!(
                report,
                "crates/{}:{} in `{}`: `{}`",
                finding.file, finding.line, finding.function, finding.what
            )
            .unwrap();
        }
    }
    assert!(
        report.is_empty(),
        "hand-rolled byte layout outside generated code; give the record a BinSchema \
         type (crates/wheat-core/schemas) instead:\n{report}"
    );
    // Every listed exception still matches something, so the list cannot rot.
    for (file, function, _) in NOT_PERSISTED {
        assert!(
            findings
                .iter()
                .any(|finding| finding.file == *file && finding.function == *function),
            "stale NOT_PERSISTED entry {file} `{function}`"
        );
    }
}

fn collect_sources(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            let name = path.file_name().unwrap();
            if name != "generated" && name != "tests" {
                collect_sources(&path, files);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

struct Finding {
    file: String,
    function: String,
    line: usize,
    what: String,
}

struct Finder<'a> {
    file: &'a str,
    functions: Vec<String>,
    findings: &'a mut Vec<Finding>,
}

impl Finder<'_> {
    fn record(&mut self, span: proc_macro2::Span, what: String) {
        self.findings.push(Finding {
            file: self.file.to_owned(),
            function: self.functions.last().cloned().unwrap_or_default(),
            line: span.start().line,
            what,
        });
    }
}

fn is_test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        if attribute.path().is_ident("test") {
            return true;
        }
        if !attribute.path().is_ident("cfg") {
            return false;
        }
        let mut test = false;
        let _ = attribute.parse_nested_meta(|meta| {
            test |= meta.path.is_ident("test");
            Ok(())
        });
        test
    })
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if !is_test_only(&item.attrs) {
            visit::visit_item_mod(self, item);
        }
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if !is_test_only(&item.attrs) {
            visit::visit_item_impl(self, item);
        }
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if is_test_only(&item.attrs) {
            return;
        }
        self.functions.push(item.sig.ident.to_string());
        visit::visit_item_fn(self, item);
        self.functions.pop();
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if is_test_only(&item.attrs) {
            return;
        }
        self.functions.push(item.sig.ident.to_string());
        visit::visit_impl_item_fn(self, item);
        self.functions.pop();
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        self.functions.push(item.sig.ident.to_string());
        visit::visit_trait_item_fn(self, item);
        self.functions.pop();
    }

    fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
        if is_test_only(&item.attrs) {
            return;
        }
        self.functions.push(item.ident.to_string());
        visit::visit_item_const(self, item);
        self.functions.pop();
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if !is_test_only(&item.attrs) {
            visit::visit_item_use(self, item);
        }
    }

    fn visit_stmt(&mut self, statement: &'ast syn::Stmt) {
        let attributes = match statement {
            syn::Stmt::Local(local) => &local.attrs,
            syn::Stmt::Macro(statement) => &statement.attrs,
            _ => &Vec::new(),
        };
        if !is_test_only(attributes) {
            visit::visit_stmt(self, statement);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if is_test_only(&call.attrs) {
            return;
        }
        let method = call.method.to_string();
        if CONVERSIONS.contains(&method.as_str()) {
            self.record(call.method.span(), format!(".{method}()"));
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        if let Some(segment) = path.segments.last() {
            let name = segment.ident.to_string();
            if CONVERSIONS.contains(&name.as_str()) {
                self.record(segment.ident.span(), name);
            }
        }
        if path
            .segments
            .first()
            .is_some_and(|segment| segment.ident == "byteorder")
        {
            self.record(path.segments[0].ident.span(), "byteorder".into());
        }
        visit::visit_path(self, path);
    }

    fn visit_use_path(&mut self, path: &'ast syn::UsePath) {
        if path.ident == "byteorder" {
            self.record(path.ident.span(), "use byteorder".into());
        }
        visit::visit_use_path(self, path);
    }

    fn visit_expr(&mut self, expression: &'ast syn::Expr) {
        let test_only = match expression {
            syn::Expr::If(expression) => is_test_only(&expression.attrs),
            syn::Expr::Block(expression) => is_test_only(&expression.attrs),
            syn::Expr::Call(expression) => is_test_only(&expression.attrs),
            _ => false,
        };
        if !test_only {
            visit::visit_expr(self, expression);
        }
    }
}
