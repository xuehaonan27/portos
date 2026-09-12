//! The fs driver against a real tree, through a real kernel.
//!
//! What is worth asserting here is not that reading a file works — it is the
//! discipline around it: a big file must not arrive in the context, a listing
//! must not include `target/`, and a path that climbs out of the tree must be
//! refused rather than quietly resolved.

use portos_abi::ids::{PluginName, Verb};
use portos_abi::wire::Payload;
use portos_kernel::Kernel;
use portos_kernel::host::Host;
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const FS_BIN: &str = env!("CARGO_BIN_EXE_portos-fs");

struct Fixture {
    kernel: Arc<Kernel>,
    host: Host,
    plugin: PluginName,
    root: PathBuf,
    tree: PathBuf,
}

impl Fixture {
    /// A kernel root and, separately, the tree the driver serves — kept apart
    /// so the driver never sees the kernel's own sqlite and object store.
    fn open(tag: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("portos-fs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tree = root.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let kernel = Arc::new(Kernel::open(&root).unwrap());
        let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
        let plugin = host
            .spawn(
                Path::new(FS_BIN),
                &[],
                &[(
                    "PORTOS_PLUGIN_CONFIG",
                    &format!(
                        "{{\"root\":{}}}",
                        serde_json::to_string(tree.to_str().unwrap()).unwrap()
                    ),
                )],
            )
            .unwrap();
        Fixture {
            kernel,
            host,
            plugin,
            root,
            tree,
        }
    }

    fn write_file(&self, rel: &str, body: &str) {
        let p = self.tree.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn call(&self, verb: &str, args: Value) -> Result<Value, String> {
        self.host
            .call(
                &self.plugin,
                &Verb::parse(verb).expect("test verb"),
                Payload::of(&args).expect("test payload"),
            )
            .map(|p| p.parse().expect("json"))
            .map_err(|e| e.to_string())
    }

    /// The bytes behind a handle, straight from the CAS.
    fn artifact(&self, handle: &str) -> String {
        let mut f = self.kernel.cas.open_read(&handle.to_string()).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        s
    }

    fn close(self) {
        self.host.shutdown_all();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn reads_and_writes_a_file_in_the_tree() {
    let fx = Fixture::open("rw");

    let wrote = fx
        .call(
            "fs::write",
            json!({"path": "src/main.rs", "content": "fn main() {}\n", "create_dirs": true}),
        )
        .expect("write");
    assert_eq!(wrote["bytes"], 13);

    let read = fx.call("fs::read", json!({"path": "src/main.rs"})).unwrap();
    assert_eq!(read["text"], "fn main() {}\n", "small files stay inline");

    // A byte range, for paging through something long.
    let part = fx
        .call(
            "fs::read",
            json!({"path": "src/main.rs", "offset": 3, "len": 4}),
        )
        .unwrap();
    assert_eq!(part["text"], "main");

    fx.close();
}

/// The claim these drivers exist to make. A file the model may not even want
/// must not land in its context; the handle is the whole of what it costs,
/// and the bytes behind it must be exactly the file.
#[test]
fn a_big_file_arrives_as_a_handle_not_as_context() {
    let fx = Fixture::open("bulk");
    let body = "0123456789abcdef\n".repeat(4096); // ~68KB, well over the line
    fx.write_file("big.log", &body);

    let (context_before, _) = fx.host.meter();
    let read = fx.call("fs::read", json!({"path": "big.log"})).unwrap();

    assert!(read["text"].is_null(), "not inline: {}", read);
    let handle = read["handle"].as_str().expect("a handle");
    assert_eq!(read["size"].as_u64(), Some(body.len() as u64));
    let preview = read["preview"].as_str().expect("a preview");
    assert!(
        preview.len() < body.len() / 10,
        "the preview is bounded, not the file: {} of {}",
        preview.len(),
        body.len()
    );
    assert!(body.starts_with(preview), "the preview is the head of it");

    assert_eq!(
        fx.artifact(handle),
        body,
        "the artifact is the file, exactly"
    );

    // The measurement, not just the shape: the reply cost context in the
    // hundreds of bytes while the file moved on the data plane.
    let (context_after, data_after) = fx.host.meter();
    let context = context_after - context_before;
    assert!(
        data_after >= body.len() as u64,
        "the bytes went through the data plane: {data_after}"
    );
    assert!(
        context < body.len() as u64 / 8,
        "context spent {context}B on a {}B file",
        body.len()
    );

    fx.close();
}

#[test]
fn lists_globs_and_greps_while_honouring_gitignore() {
    let fx = Fixture::open("search");
    fx.write_file(".gitignore", "target/\n");
    fx.write_file("src/a.rs", "fn alpha() {}\nlet x = 1;\n");
    fx.write_file("src/b.rs", "fn beta() {}\n");
    fx.write_file("notes.md", "alpha is a greek letter\n");
    fx.write_file("target/debug/a.rs", "fn alpha_generated() {}\n");

    let listed = fx.call("fs::list", json!({"depth": 3})).unwrap();
    let paths: Vec<&str> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["path"].as_str())
        .collect();
    assert!(paths.contains(&"src/a.rs"), "{paths:?}");
    assert!(
        !paths.iter().any(|p| p.starts_with("target/")),
        "an ignored tree is not something anyone asked to see: {paths:?}"
    );

    let globbed = fx
        .call("fs::glob", json!({"pattern": "src/**/*.rs"}))
        .unwrap();
    assert_eq!(globbed["paths"].as_array().unwrap().len(), 2, "{globbed}");

    let all = fx.call("fs::grep", json!({"pattern": "alpha"})).unwrap();
    let hits: Vec<&str> = all["matches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["path"].as_str())
        .collect();
    assert!(
        hits.contains(&"src/a.rs") && hits.contains(&"notes.md"),
        "{hits:?}"
    );
    assert!(!hits.contains(&"target/debug/a.rs"), "ignored: {hits:?}");

    let narrowed = fx
        .call("fs::grep", json!({"pattern": "alpha", "glob": "**/*.rs"}))
        .unwrap();
    let matches = narrowed["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{narrowed}");
    assert_eq!(matches[0]["line"], 1, "matches carry a line number");

    fx.close();
}

/// The way out of the store.
///
/// Everything so far pushed bytes *into* the CAS; getting them back out
/// meant reading them into the conversation first. Writing an artifact to a
/// file is the missing direction, and it copies file to file — the bytes
/// never enter this process, let alone the model's context.
#[test]
fn an_artifact_can_be_written_out_without_being_read_in() {
    let fx = Fixture::open("out");
    let body = "line\n".repeat(8192); // ~40KB, comfortably over the line
    fx.write_file("source.log", &body);

    let stored = fx.call("fs::read", json!({"path": "source.log"})).unwrap();
    let handle = stored["handle"].as_str().expect("a handle").to_string();

    let (context_before, _) = fx.host.meter();
    let wrote = fx
        .call(
            "fs::write",
            json!({"path": "copy/out.log", "artifact": handle, "create_dirs": true}),
        )
        .expect("written from the store");
    assert_eq!(wrote["bytes"].as_u64(), Some(body.len() as u64));
    assert_eq!(
        std::fs::read_to_string(fx.tree.join("copy/out.log")).unwrap(),
        body,
        "byte for byte"
    );
    assert!(
        fx.host.meter().0 - context_before < 2048,
        "and the conversation never carried it"
    );

    // A file in someone's tree is not a store object: it must be writable
    // again, or the next tool to touch it fails for a reason nobody can see.
    let mode = std::fs::metadata(fx.tree.join("copy/out.log"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o644, "written out as an ordinary file");

    // One source or the other, never both and never neither.
    assert!(
        fx.call("fs::write", json!({"path": "x"}))
            .unwrap_err()
            .contains("exactly one")
    );

    fx.close();
}

/// Not a security boundary — that line was retired — but a path that leaves
/// the tree is a mistake, and a mistake should be an error rather than a
/// file.
#[test]
fn a_path_that_leaves_the_tree_is_refused() {
    let fx = Fixture::open("scope");
    fx.write_file("inside.txt", "ok\n");

    for bad in ["../outside.txt", "/etc/hostname", "a/../../b"] {
        let err = fx
            .call("fs::read", json!({"path": bad}))
            .expect_err(&format!("{bad} should be refused"));
        assert!(
            err.contains("root") || err.contains("relative"),
            "the refusal says why: {err}"
        );
    }
    assert!(fx.call("fs::read", json!({"path": "inside.txt"})).is_ok());

    fx.close();
}
