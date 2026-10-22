//! The comp's demo repository, `tessera`: what `--fixture` shows and what the
//! screenshot and click tests draw. Kept to the comp's own data so a capture can
//! be held up against `Wheat Client.dc.html`.

use crate::model::{
    Agent, Change, Diff, DiffLine, FileCode, FileEntry, FileGroup, FrontierChoice, LineKind, Model,
    Revision, StoreSummary, WorkspaceEntry, WorkspaceState, WorkspaceView,
};

struct Demo {
    short: &'static str,
    title: &'static str,
    author: &'static str,
    age: &'static str,
    messages: &'static [&'static str],
    revisions: usize,
    depends_on: &'static [&'static str],
    independent: bool,
    published: bool,
    conflicted: bool,
}

const fn demo(
    short: &'static str,
    title: &'static str,
    author: &'static str,
    age: &'static str,
    revisions: usize,
    messages: &'static [&'static str],
) -> Demo {
    Demo {
        short,
        title,
        author,
        age,
        messages,
        revisions,
        depends_on: &[],
        independent: false,
        published: false,
        conflicted: false,
    }
}

const AGENTS: [&str; 3] = ["claude-a1", "codex-b2", "claude-c3"];

fn changes() -> Vec<Demo> {
    vec![
        demo(
            "c1d9",
            "Docs: frontier and integrate guide",
            "claude-c3",
            "4m",
            1,
            &[],
        ),
        Demo {
            depends_on: &["k2f0"],
            ..demo(
                "k3a7",
                "Resolver: key scopes by Symbol",
                "claude-a1",
                "6m",
                2,
                &[
                    "Resolver: key scopes by Symbol",
                    "Fix shadowing test after Symbol keys",
                ],
            )
        },
        Demo {
            depends_on: &["k1e4"],
            ..demo(
                "k2f0",
                "Parser: use interned symbols for idents",
                "claude-a1",
                "11m",
                2,
                &[
                    "Parser: use interned symbols for idents",
                    "Thread &Interner through Parser::new",
                ],
            )
        },
        Demo {
            depends_on: &["k1e4"],
            ..demo(
                "k4b2",
                "Diagnostics: print symbol names via table",
                "codex-b2",
                "14m",
                1,
                &[],
            )
        },
        demo(
            "k1e4",
            "Lexer: intern identifiers in a shared table",
            "you",
            "22m",
            3,
            &[
                "Lexer: intern identifiers in a shared table",
                "Switch table to FxHashMap; add Symbol newtype",
                "Intern keywords too; drop legacy_ident",
            ],
        ),
        Demo {
            depends_on: &["b1a5"],
            conflicted: true,
            ..demo(
                "b20c",
                "Cache: evict by generation",
                "codex-b2",
                "31m",
                2,
                &[
                    "Cache: evict by generation",
                    "Keep pinned queries across generations",
                ],
            )
        },
        Demo {
            independent: true,
            ..demo(
                "b1a5",
                "Cache: content-addressed query cache",
                "codex-b2",
                "48m",
                4,
                &[
                    "Cache: content-addressed query cache",
                    "Hash inputs with blake3",
                    "Bound cache by entry count",
                    "Add hit/miss counters",
                ],
            )
        },
        Demo {
            published: true,
            ..demo("8b20", "Release 0.14.0", "you", "2d", 1, &[])
        },
        Demo {
            published: true,
            ..demo(
                "71cc",
                "CI: run miri on unsafe modules",
                "you",
                "3d",
                3,
                &[],
            )
        },
        Demo {
            published: true,
            ..demo(
                "5f02",
                "Span: store file id as u32",
                "m.okafor",
                "4d",
                2,
                &[],
            )
        },
        Demo {
            published: true,
            ..demo(
                "42e9",
                "Lexer: handle CRLF in raw strings",
                "m.okafor",
                "5d",
                1,
                &[],
            )
        },
        Demo {
            published: true,
            ..demo(
                "3e91",
                "Import from git: main@3e91c0a",
                "git import",
                "6d",
                1,
                &[],
            )
        },
    ]
}

pub fn model() -> Model {
    let demos = changes();
    let index_of = |short: &str| demos.iter().position(|demo| demo.short == short);
    let mut changes: Vec<Change> = demos
        .iter()
        .map(|demo| Change {
            id: format!("{}0000-0000-0000-0000-000000000000", demo.short),
            short: demo.short.to_owned(),
            title: demo.title.to_owned(),
            author: demo.author.to_owned(),
            agent: AGENTS.contains(&demo.author),
            age: demo.age.to_owned(),
            revision_count: demo.revisions,
            revisions: (1..=demo.revisions)
                .map(|number| Revision {
                    id: format!("{}-r{number}", demo.short),
                    message: demo.messages.get(number - 1).map_or_else(
                        || {
                            if number == 1 {
                                demo.title.to_owned()
                            } else {
                                format!("Revision {number}")
                            }
                        },
                        |message| (*message).to_owned(),
                    ),
                    actor: demo.author.to_owned(),
                    age: if number == demo.revisions {
                        demo.age.to_owned()
                    } else {
                        String::new()
                    },
                })
                .collect(),
            depends_on: demo
                .depends_on
                .iter()
                .filter_map(|short| index_of(short))
                .collect(),
            dependents: Vec::new(),
            independent: demo.independent,
            published: demo.published,
            conflicted: demo.conflicted,
        })
        .collect();
    let selected = index_of("k1e4");
    let k3 = index_of("k3a7");
    let b2 = index_of("b20c");
    let c1 = index_of("c1d9");
    let unpublished: Vec<&Change> = changes.iter().filter(|change| !change.published).collect();
    let store = StoreSummary {
        changes: 2184,
        unpublished_changes: unpublished.len(),
        unpublished_revisions: unpublished
            .iter()
            .map(|change| change.revisions.len())
            .sum(),
        conflicted_changes: changes.iter().filter(|change| change.conflicted).count(),
        conflicted_paths: 1,
    };
    let workspaces = vec![
        WorkspaceEntry {
            name: "tessera".into(),
            frontier: "main/f2".into(),
            selected,
            current: true,
            agent: None,
            state: WorkspaceState::Idle,
            task: Some("Editing locally".into()),
        },
        WorkspaceEntry {
            name: "claude-a1".into(),
            frontier: "main/f2".into(),
            selected: k3,
            current: false,
            agent: Some(Agent { hue: 280.0 }),
            state: WorkspaceState::Running,
            task: Some("Migrating resolver to Symbol keys".into()),
        },
        WorkspaceEntry {
            name: "codex-b2".into(),
            frontier: "main/f3".into(),
            selected: b2,
            current: false,
            agent: Some(Agent { hue: 165.0 }),
            state: WorkspaceState::Running,
            task: Some("Generation-based cache eviction".into()),
        },
        WorkspaceEntry {
            name: "claude-c3".into(),
            frontier: "main/f1".into(),
            selected: c1,
            current: false,
            agent: Some(Agent { hue: 320.0 }),
            state: WorkspaceState::Waiting,
            task: Some("Waiting for review".into()),
        },
    ];
    let file = |path: &str, code: FileCode, group: FileGroup, stat: &str| FileEntry {
        path: path.to_owned(),
        code,
        group,
        stat: stat.to_owned(),
    };
    let files = vec![
        file(
            "src/lexer/intern.rs",
            FileCode::Modified,
            FileGroup::Tracked,
            "+38 −11",
        ),
        file(
            "src/lexer/mod.rs",
            FileCode::Modified,
            FileGroup::Tracked,
            "+6 −2",
        ),
        file(
            "src/lexer/token.rs",
            FileCode::Modified,
            FileGroup::Tracked,
            "+14 −9",
        ),
        file(
            "benches/lex.rs",
            FileCode::Modified,
            FileGroup::Tracked,
            "+22",
        ),
        file(
            "src/lexer/legacy_ident.rs",
            FileCode::Deleted,
            FileGroup::Tracked,
            "−142",
        ),
        file(
            "scripts/gen-keywords.sh",
            FileCode::TypeChanged,
            FileGroup::Tracked,
            "file → exec",
        ),
        file(
            "docs/img/lexer-pipeline.png",
            FileCode::Modified,
            FileGroup::Tracked,
            "binary",
        ),
        file(
            "src/lexer/symbol.rs",
            FileCode::Untracked,
            FileGroup::Untracked,
            "+61",
        ),
        file(
            "tests/intern_roundtrip.rs",
            FileCode::Untracked,
            FileGroup::Untracked,
            "+44",
        ),
        file("target/", FileCode::Ignored, FileGroup::Ignored, ""),
        file(".wheat/cache/", FileCode::Ignored, FileGroup::Ignored, ""),
        file("perf.data", FileCode::Ignored, FileGroup::Ignored, ""),
    ];
    let mut model = Model {
        name: "tessera".into(),
        path: "~/src/tessera".into(),
        workspaces,
        store,
        workspace: WorkspaceView {
            label: "main".into(),
            frontier: Some("f2".into()),
            frontiers: vec![
                FrontierChoice {
                    name: "f1".into(),
                    changes: Some(1),
                },
                FrontierChoice {
                    name: "f2".into(),
                    changes: Some(4),
                },
                FrontierChoice {
                    name: "f3".into(),
                    changes: Some(2),
                },
            ],
            selected,
            moved: Some("codex-b2 · b20c r3 · 40 s ago".into()),
            files,
        },
        changes: Vec::new(),
        older_changes: 2172,
    };
    std::mem::swap(&mut model.changes, &mut changes);
    model.index_dependents();
    model
}

/// The comp's diffs: its hand-written one for `intern.rs`, a new file's for an
/// untracked path, the opaque cards for the rest, and one generic hunk.
pub fn workspace_diff(model: &Model, path: &str) -> Diff {
    let Some(file) = model.workspace.files.iter().find(|file| file.path == path) else {
        return Diff::Failed(format!("{path} is no longer among the workspace's edits"));
    };
    let facts = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    };
    if path.ends_with(".png") {
        return Diff::Opaque {
            kind: "binary".into(),
            facts: facts(&[
                ("size", "48.2 KB → 51.0 KB"),
                ("mode", "100644"),
                ("content", "blake3 4e1a…90c2 → 7c03…11fe"),
            ]),
        };
    }
    match file.code {
        FileCode::TypeChanged => Diff::Opaque {
            kind: "type change".into(),
            facts: facts(&[
                ("type", "file → executable"),
                ("mode", "100644 → 100755"),
                ("content", "unchanged"),
            ]),
        },
        FileCode::Deleted => Diff::Opaque {
            kind: "deleted".into(),
            facts: facts(&[
                ("was", "142 lines · 4.3 KB"),
                ("last change", "5f02 · m.okafor"),
            ]),
        },
        FileCode::Ignored => Diff::Opaque {
            kind: "ignored".into(),
            facts: facts(&[(
                "rule",
                if path == "perf.data" {
                    "perf.data*"
                } else {
                    path
                },
            )]),
        },
        FileCode::Untracked => Diff::Text { lines: new_file() },
        FileCode::Modified if path == "src/lexer/intern.rs" => Diff::Text {
            lines: intern_diff(),
        },
        FileCode::Modified => Diff::Text {
            lines: generic_diff(path),
        },
    }
}

pub fn revision_diff(model: &Model, change: usize, _revision: usize) -> Diff {
    match model.changes.get(change) {
        Some(change) if change.short == "k1e4" => Diff::Text {
            lines: intern_diff(),
        },
        Some(change) => {
            let area = change
                .title
                .split(':')
                .next()
                .unwrap_or_default()
                .to_lowercase();
            Diff::Text {
                lines: generic_diff(&format!("{area}/mod.rs")),
            }
        }
        None => Diff::None,
    }
}

fn lines(rows: &[(char, u32, u32, &str)]) -> Vec<DiffLine> {
    rows.iter()
        .map(|&(kind, old, new, text)| DiffLine {
            kind: match kind {
                'h' => LineKind::Hunk,
                'a' => LineKind::Added,
                'd' => LineKind::Removed,
                _ => LineKind::Context,
            },
            old: (old != 0).then_some(old),
            new: (new != 0).then_some(new),
            text: text.to_owned(),
        })
        .collect()
}

fn intern_diff() -> Vec<DiffLine> {
    lines(&[
        (
            'h',
            0,
            0,
            "@@ -10,14 +10,21 @@ use std::collections::HashMap;",
        ),
        ('c', 10, 10, "use crate::span::Span;"),
        ('d', 11, 0, "use std::collections::HashMap;"),
        ('a', 0, 11, "use rustc_hash::FxHashMap;"),
        ('a', 0, 12, "use bumpalo::Bump;"),
        ('c', 12, 13, ""),
        ('c', 13, 14, "#[derive(Default)]"),
        ('c', 14, 15, "pub struct Interner {"),
        ('d', 15, 0, "    map: HashMap<String, u32>,"),
        ('a', 0, 16, "    map: FxHashMap<&'static str, Symbol>,"),
        ('c', 16, 17, "    strings: Vec<&'static str>,"),
        ('a', 0, 18, "    arena: Bump,"),
        ('c', 17, 19, "}"),
        ('c', 18, 20, ""),
        ('c', 19, 21, "impl Interner {"),
        ('d', 20, 0, "    pub fn intern(&mut self, s: &str) -> u32 {"),
        (
            'a',
            0,
            22,
            "    pub fn intern(&mut self, s: &str) -> Symbol {",
        ),
        ('c', 21, 23, "        if let Some(&id) = self.map.get(s) {"),
        ('c', 22, 24, "            return id;"),
        ('c', 23, 25, "        }"),
        ('d', 24, 0, "        let id = self.strings.len() as u32;"),
        ('d', 25, 0, "        self.map.insert(s.to_owned(), id);"),
        (
            'a',
            0,
            26,
            "        let s: &'static str = unsafe { &*(self.arena.alloc_str(s) as *const str) };",
        ),
        (
            'a',
            0,
            27,
            "        let id = Symbol(self.strings.len() as u32);",
        ),
        ('a', 0, 28, "        self.map.insert(s, id);"),
        ('a', 0, 29, "        self.strings.push(s);"),
        ('c', 26, 30, "        id"),
        ('c', 27, 31, "    }"),
        ('a', 0, 32, ""),
        (
            'a',
            0,
            33,
            "    pub fn resolve(&self, sym: Symbol) -> &str {",
        ),
        ('a', 0, 34, "        self.strings[sym.0 as usize]"),
        ('a', 0, 35, "    }"),
        ('c', 28, 36, "}"),
    ])
}

fn generic_diff(path: &str) -> Vec<DiffLine> {
    let name = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .split('.')
        .next()
        .unwrap_or_default();
    let mut out = lines(&[
        ('h', 0, 0, ""),
        ('c', 31, 31, "    fn next_token(&mut self) -> Token {"),
        ('c', 32, 32, "        let start = self.pos;"),
        (
            'd',
            33,
            0,
            "        let text = self.slice(start).to_string();",
        ),
        ('a', 0, 33, "        let text = self.slice(start);"),
        ('a', 0, 34, "        let sym = self.interner.intern(text);"),
        ('c', 34, 35, "        match self.peek() {"),
        ('d', 35, 0, "            _ => Token::Ident(text),"),
        ('a', 0, 36, "            _ => Token::Ident(sym),"),
        ('c', 36, 37, "        }"),
        (
            'a',
            0,
            38,
            "        // keywords are pre-interned; see gen-keywords.sh",
        ),
        ('c', 37, 39, "    }"),
    ]);
    out[0].text = format!("@@ -31,8 +31,10 @@ impl {name}");
    out
}

fn new_file() -> Vec<DiffLine> {
    let body = [
        "use crate::lexer::Interner;",
        "",
        "#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]",
        "pub struct Symbol(pub(crate) u32);",
        "",
        "impl Symbol {",
        "    pub fn as_str<'a>(self, i: &'a Interner) -> &'a str {",
        "        i.resolve(self)",
        "    }",
        "}",
    ];
    let mut out = vec![DiffLine {
        kind: LineKind::Hunk,
        old: None,
        new: None,
        text: format!("@@ -0,0 +1,{} @@ new file", body.len()),
    }];
    out.extend(body.iter().enumerate().map(|(index, text)| DiffLine {
        kind: LineKind::Added,
        old: None,
        new: Some(index as u32 + 1),
        text: (*text).to_owned(),
    }));
    out
}
