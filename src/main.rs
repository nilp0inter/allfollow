mod cli_args;
mod flake_lock;
mod fmt_colors;

use std::io::Write;
use std::iter::repeat;

use bpaf::Bpaf;
use cli_args::{Input, Output};
use flake_lock::{
    LockFile, Node, NodeEdge, NodeEdgeRef as _, MAX_SUPPORTED_LOCK_VERSION,
    MIN_SUPPORTED_LOCK_VERSION,
};
use indexmap::IndexMap;
use owo_colors::OwoColorize;
use serde::Serialize;
use serde_json::Serializer;

static EXPECT_ROOT_EXIST: &str = "the root node to exist";

/// Imitate Nix flake input following behavior as a post-process,
/// so that you can stop manually maintaining tedious connections
/// between many flake inputs.
/// This small tool aims to replace every instance of
/// `inputs.*.inputs.*.follows = "*";` in your `flake.nix` with automation.
#[derive(Debug, Clone, Bpaf)]
#[bpaf(options, generate(parse_command_env_args))]
enum Command {
    #[bpaf(command("prune"))]
    Prune {
        /// Do not imitate `inputs.*.follows`, reference node indices instead
        #[bpaf(long, long("indexed"))]
        no_follows: bool,
        /// Do not minify the output JSON
        #[bpaf(short('p'), long)]
        pretty: bool,
        //
        #[bpaf(external(output_options))]
        output_opts: OutputOptions,
        /// The path of `flake.lock` to read, or `-` to read from standard input.
        /// If unspecified, defaults to the current directory.
        #[bpaf(positional("INPUT"), fallback(Input::from("./flake.lock")))]
        lock_file: Input,
    },
    #[bpaf(command("count"))]
    Count {
        /// Show the data as JSON.
        #[bpaf(short('j'), long)]
        json: bool,
        /// Do not minify the output JSON
        #[bpaf(short('p'), long)]
        pretty: bool,
        //
        #[bpaf(external(output_options))]
        output_opts: OutputOptions,
        /// The path of `flake.lock` to read, or `-` to read from standard input.
        /// If unspecified, defaults to the current directory.
        #[bpaf(positional("INPUT"), fallback(Input::from("./flake.lock")))]
        lock_file: Input,
    },
    #[bpaf(command("config"))]
    Config {
        /// Modify the `flake.nix` file in the same directory as the lock file.
        #[bpaf(short('I'), long)]
        in_place: bool,
        /// The path of `flake.lock` to read, or `-` to read from standard input.
        /// If unspecified, defaults to the current directory.
        #[bpaf(positional("INPUT"), fallback(Input::from("./flake.lock")))]
        lock_file: Input,
    },
}

/// Generic options for output handling:
#[derive(Debug, Clone, Bpaf)]
struct OutputOptions {
    /// Write new file back to `INPUT` (if specified)
    #[bpaf(short('I'), long)]
    in_place: bool,
    /// Overwrite the output file if it exists
    #[bpaf(short('f'), long, long("force"))]
    overwrite: bool,
    /// Path of the file to write, set to `-` for stdout (default)
    #[bpaf(short('o'), long, argument("OUTPUT"), fallback(Output::Stdout))]
    output: Output,
}

impl Command {
    fn from_env() -> Self {
        let mut args = parse_command_env_args().run();
        #[allow(clippy::single_match)]
        match &mut args {
            Command::Prune {
                lock_file,
                output_opts,
                ..
            }
            | Command::Count {
                lock_file,
                output_opts,
                ..
            } => {
                if output_opts.in_place {
                    output_opts.output = Output::from(lock_file.clone());
                    output_opts.overwrite = true;
                }
            }
            Command::Config { .. } => {}
        };
        args
    }
}

fn main() {
    match Command::from_env() {
        Command::Prune {
            no_follows,
            lock_file,
            pretty,
            output_opts:
                OutputOptions {
                    in_place: _,
                    overwrite,
                    output,
                },
        } => {
            let mut lock = read_flake_lock(lock_file);

            let node_hits = FlakeNodeVisits::count_from_index(&lock, lock.root_index());
            eprintln!();
            elogln!(:bold :bright_magenta "Flake input nodes' reference counts:"; &node_hits);

            substitute_flake_inputs_with_follows(&lock, no_follows);
            eprintln!();
            prune_orphan_nodes(&mut lock);

            eprintln!();
            let node_hits = FlakeNodeVisits::count_from_index(&lock, lock.root_index());
            elog!(
                :bold (:bright_magenta "Flake input nodes' reference counts", :bright_green "after successful pruning" :bright_magenta ":");
                &node_hits
            );
            eprintln!();

            serialize_to_json_output(&lock, output, overwrite, pretty)
        }
        Command::Count {
            json,
            pretty,
            lock_file,
            output_opts:
                OutputOptions {
                    in_place: _,
                    overwrite,
                    output,
                },
        } => {
            let lock = read_flake_lock(lock_file);
            let node_hits = FlakeNodeVisits::count_from_index(&lock, lock.root_index());
            if json {
                serialize_to_json_output(&*node_hits, output, overwrite, pretty)
            } else {
                logln!(:bold :bright_magenta "Flake input nodes' reference counts:"; &node_hits)
            }
        }
        Command::Config {
            in_place,
            lock_file,
        } => {
            let lock = read_flake_lock(lock_file.clone());

            let mut buf = Vec::new();
            print_flake_follows_config(&lock, &mut buf);
            let config_output = String::from_utf8(buf).expect("config output to be utf8");

            if in_place {
                match lock_file {
                    Input::File(path) => {
                        let flake_nix_path = path
                            .parent()
                            .expect("lock file to have a parent directory")
                            .join("flake.nix");
                        update_flake_nix(&flake_nix_path, &config_output);
                    }
                    Input::Stdin => {
                        // For stdin, we default to current directory for flake.nix
                        let flake_nix_path = std::path::PathBuf::from("flake.nix");
                        update_flake_nix(&flake_nix_path, &config_output);
                    }
                }
            } else {
                print!("{}", config_output);
            }
        }
    }
}

fn update_flake_nix(path: &std::path::Path, config: &str) {
    let content = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("Failed to read flake.nix at {:?}: {}", path, e);
    });

    let start_marker = "# START INPUT FOLLOW BLOCK -- DO NOT EDIT MANUALLY";
    let end_marker = "# END INPUT FOLLOW BLOCK -- DO NOT EDIT MANUALLY";

    // Find markers
    let start_idx = content.find(start_marker);
    let end_idx = content.find(end_marker);

    match (start_idx, end_idx) {
        (Some(start), Some(end)) => {
            if start >= end {
                panic!("Found markers in flake.nix but START comes after END.");
            }

            // Find the start of the line for indentation
            let line_start = content[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let indent = &content[line_start..start];
            // Check if indent is only whitespace
            if !indent.trim().is_empty() {
                 // If not whitespace, maybe the marker is inline?
                 // But requirements said "respect the identation".
                 // We'll assume the indent is everything from last newline.
            }

            // Prepare the new block with indentation
            let indented_config = config
                .lines()
                .map(|line| format!("{}{}", indent, line))
                .collect::<Vec<_>>()
                .join("\n");

            // We replace everything from start_marker to end_marker + end_marker.len()
            // The `config` generated by `print_flake_follows_config` will already include markers?
            // Wait, my plan said `print_flake_follows_config` wraps it in markers.

            // If `config` has markers, I should not double them.
            // But here I'm replacing from existing marker to existing marker.

            // Let's refine `print_flake_follows_config` first to see what it outputs.
            // If `print_flake_follows_config` outputs:
            // # START...
            // inputs = { ... };
            // # END...

            // And I replace:
            // <indent># START...
            // ...
            // <indent># END...

            // with `indented_config`.

            // `indented_config` will be:
            // <indent># START...
            // <indent>inputs = { ... };
            // <indent># END...

            // This looks correct.

            let end_of_end_marker = end + end_marker.len();

            // We need to be careful about what we replace.
            // `start` points to `# START`.
            // `end` points to `# END`.

            // If I replace `content[start..end_of_end_marker]` with `indented_config`.

            let mut new_content = String::with_capacity(content.len());
            new_content.push_str(&content[..start]);
            // Remove the indentation that was part of the original file if we are re-adding it?
            // No, `content[..start]` includes the `indent` because `start` is where `#` is.
            // So `content[..start]` ends with `indent`.
            // If `indented_config` starts with `indent`, we get double indentation for the first line.

            // Correction:
            // `content[..start]` ends with `indent`.
            // `indented_config` starts with `indent`.
            // We should trim the tail of `content[..start]`?

            // Or better: `print_flake_follows_config` returns the block WITHOUT indentation.
            // We add indentation here.

            // BUT `print_flake_follows_config` is also used for stdout.
            // For stdout, indentation 0 is fine.

            // If `indented_config` is:
            // indent + "# START..."
            // indent + "inputs..."

            // And `content[..start]` is `...newline + indent`.
            // Replaced: `...newline + indent + indent + "# START..."`. Double indent.

            // So we should replace `content[line_start..end_of_end_marker]`.
            // `line_start` is start of the line containing `# START`.

            new_content.truncate(line_start);
            new_content.push_str(&indented_config);
            new_content.push_str(&content[end_of_end_marker..]);

            std::fs::write(path, new_content).unwrap_or_else(|e| {
                panic!("Failed to write to flake.nix: {}", e);
            });
            eprintln!("Successfully updated flake.nix");
        }
        _ => {
            panic!("Could not find the start and end markers in flake.nix. Please add them manually:\n{}\ninputs = {{ ... }};\n{}", start_marker, end_marker);
        }
    }
}

fn read_flake_lock(lock_file: Input) -> LockFile {
    let reader = lock_file
        .open()
        .unwrap_or_else(|e| panic!("Failed to read the input file: {e}"));
    let deserializer = &mut serde_json::Deserializer::from_reader(reader);

    let lock: LockFile = {
        serde_path_to_error::deserialize(deserializer)
            .unwrap_or_else(|e| panic!("Failed to deserialize the provided flake lock: {e}"))
    };

    if lock.version() < MIN_SUPPORTED_LOCK_VERSION && lock.version() > MAX_SUPPORTED_LOCK_VERSION {
        panic!(
            "This program supports lock files between schema versions {} and {} while the flake you have asked to modify is of version {}.",
            MIN_SUPPORTED_LOCK_VERSION,
            MAX_SUPPORTED_LOCK_VERSION,
            lock.version()
        );
    }

    lock
}

fn serialize_to_json_output(value: impl Serialize, output: Output, overwrite: bool, pretty: bool) {
    let writer = output
        .create(!overwrite)
        .unwrap_or_else(|e| panic!("Could not write to output: {e}"));

    let res = if pretty {
        value.serialize(&mut Serializer::pretty(writer))
    } else {
        value.serialize(&mut Serializer::new(writer))
    };

    if let Err(e) = res {
        panic!("Failed while serializing to output, file is probably corrupt: {e}")
    }
}

fn substitute_flake_inputs_with_follows(lock: &LockFile, indexed: bool) {
    elogln!(:bold :bright_magenta "Redirecting inputs to imitate follows behavior.");

    let root = lock.root().expect(EXPECT_ROOT_EXIST);
    for (input_name, input_index) in root
        .iter_edges()
        .filter_map(|(name, edge)| edge.index().map(|index| (name, index)))
    {
        elogln!(:bold (:bright_cyan "Replacing inputs for", :green "'{input_name}'"), :dimmed "(" :dimmed :italic "'{input_index}'" :dimmed ")");
        let input = &*lock
            .get_node(&*input_index)
            .expect("a node to exist with this index");
        substitute_node_inputs_with_root_inputs(lock, input, indexed);
    }
}

/// When `indexed == false`, the input replacements all will reference identically
/// named inputs from the root node. This imitates input following behavior.
///
/// Otherwise, if `indexed == true`, the each input replacement will be cloned
/// verbatim from the root node, most likely retaining a `NodeEdge::Indexed`.
fn substitute_node_inputs_with_root_inputs(lock: &LockFile, node: &Node, indexed: bool) {
    let root = lock.root().expect(EXPECT_ROOT_EXIST);
    for (edge_name, mut edge) in node.iter_edges_mut() {
        if let Some(root_edge) = root.get_edge(edge_name) {
            if indexed {
                let old = std::mem::replace(&mut *edge, (*root_edge).clone());
                elogln!("-", :yellow "'{edge_name}'", "now references", :italic :purple "'{edge}'", :dimmed "(was '{old}')");
            } else {
                let old = std::mem::replace(&mut *edge, NodeEdge::from_iter([edge_name]));
                elogln!("-", :yellow "'{edge_name}'", "now follows", :green "'{edge}'", :dimmed "(was '{old}')");
            }
        } else {
            elogln!(
                :bold (:cyan "No suitable replacement for", :yellow "'{edge_name}'"),
                :dimmed "(" :dimmed :italic ("'" (lock.resolve_edge(&edge).unwrap()) "'") :dimmed ")"
            );
        }
    }
}

fn prune_orphan_nodes(lock: &mut LockFile) {
    elogln!(:bold :bright_magenta "Pruning orphaned nodes from modified lock.");

    let node_hits = FlakeNodeVisits::count_from_index(lock, lock.root_index());

    let dead_nodes = node_hits
        .into_inner()
        .into_iter()
        .filter(|&(_, count)| count == 0)
        .map(|(index, _)| index.to_owned())
        .collect::<Vec<_>>();

    for index in dead_nodes {
        lock.remove_node(&index);
        elogln!("- removed", :red "'{index}'");
    }
}

fn recurse_inputs(lock: &LockFile, index: String, op: &mut impl FnMut(String)) {
    let node = lock.get_node(&index).unwrap();
    op(index);
    for (_, edge) in node.iter_edges() {
        let index = lock.resolve_edge(&edge).unwrap();
        recurse_inputs(lock, index, op);
    }
}

fn print_flake_follows_config(lock: &LockFile, writer: &mut impl Write) {
    writeln!(writer, "# START INPUT FOLLOW BLOCK -- DO NOT EDIT MANUALLY").ok();
    writeln!(writer, "inputs = {{").ok();
    let root = lock.root().expect(EXPECT_ROOT_EXIST);
    // Identify root inputs
    let root_inputs: std::collections::HashSet<String> = root
        .iter_edges()
        .map(|(name, _)| name.to_string())
        .collect();

    // Start traversal from root inputs
    for (input_name, edge) in root.iter_edges() {
        if let Some(index) = edge.index() {
            traverse_and_print_config(
                lock,
                &root_inputs,
                &index,
                vec![input_name.to_string()],
                &mut vec![index.to_string()],
                writer,
            );
        }
    }
    writeln!(writer, "}};").ok();
    write!(writer, "# END INPUT FOLLOW BLOCK -- DO NOT EDIT MANUALLY").ok();
}

fn traverse_and_print_config(
    lock: &LockFile,
    root_inputs: &std::collections::HashSet<String>,
    current_node_index: &str,
    current_path: Vec<String>,
    visited_indices: &mut Vec<String>, // To detect cycles in the current path
    writer: &mut impl Write,
) {
    let node = lock.get_node(current_node_index).expect("node exists");

    for (edge_name, edge) in node.iter_edges() {
        // If the edge name matches a root input, print the config
        if root_inputs.contains(edge_name) {
            let mut config_path = current_path.clone();
            config_path.push(edge_name.to_string());
            // Construct string like B.inputs.C.inputs.nixpkgs.follows = "nixpkgs"
            // Path elements join with ".inputs."
            let path_str = config_path.join(".inputs.");
            writeln!(writer, "    {}.follows = \"{}\";", path_str, edge_name).ok();

            // If we are configuring it to follow, we essentially stop traversing this branch *as if* it was the root input.
            continue;
        }

        // If not following a root input, we recurse.
        if let Some(child_index) = lock.resolve_edge(&edge) {
            if !visited_indices.contains(&child_index) {
                visited_indices.push(child_index.clone());
                let mut new_path = current_path.clone();
                new_path.push(edge_name.to_string());
                traverse_and_print_config(
                    lock,
                    root_inputs,
                    &child_index,
                    new_path,
                    visited_indices,
                    writer,
                );
                visited_indices.pop();
            }
        }
    }
}


struct FlakeNodeVisits<'a> {
    inner: IndexMap<&'a str, u32>,
    // Index of the node which this count is relative to.
    root_index: &'a str,
}

impl<'a> FlakeNodeVisits<'a> {
    fn count_from_index<'new>(lock: &'new LockFile, index: &'new str) -> FlakeNodeVisits<'new> {
        let mut node_hits = IndexMap::from_iter(lock.node_indices().zip(repeat(0_u32)));
        recurse_inputs(lock, index.to_owned(), &mut |index| {
            *node_hits.get_mut(index.as_str()).unwrap() += 1;
        });
        FlakeNodeVisits {
            inner: node_hits,
            root_index: index,
        }
    }

    fn into_inner(self) -> IndexMap<&'a str, u32> {
        self.inner
    }
}

impl<'a> From<FlakeNodeVisits<'a>> for IndexMap<&'a str, u32> {
    fn from(value: FlakeNodeVisits<'a>) -> Self {
        value.into_inner()
    }
}

impl<'a> std::ops::Deref for FlakeNodeVisits<'a> {
    type Target = IndexMap<&'a str, u32>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a> std::ops::DerefMut for FlakeNodeVisits<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'a> std::fmt::Display for FlakeNodeVisits<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let max_pad = {
            let (mut min_len, mut max_len) = (0, 0);
            for key in self.inner.keys() {
                min_len = std::cmp::min(min_len, key.len());
                max_len = std::cmp::max(max_len, key.len());
            }
            max_len - min_len
        };
        for (index, count) in self.inner.iter() {
            if index == &self.root_index {
                f.write_fmt(format_args_colored!(
                    :dimmed .("{:1$}", index, max_pad), :red "=", :dimmed &count;
                ))?
            } else if *count <= 1 {
                f.write_fmt(format_args_colored!(
                    :bold :bright_yellow .("{:1$}", index, max_pad), :red "=", :dimmed &count;
                ))?
            } else {
                f.write_fmt(format_args_colored!(
                    .("{:1$}", index, max_pad), :red "=", :bold :bright_green &count;
                ))?
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_json_snapshot;
    use std::fs;

    use crate::{prune_orphan_nodes, read_flake_lock, substitute_flake_inputs_with_follows};

    static HYPRLAND_LOCK_NO_FOLLOWS: &str = "samples/hyprland/no-follows/flake.lock";

    #[test]
    fn prune_hyprland_flake_lock() {
        let mut lock = read_flake_lock(HYPRLAND_LOCK_NO_FOLLOWS.into());
        substitute_flake_inputs_with_follows(&lock, false);
        prune_orphan_nodes(&mut lock);
        insta::with_settings!(
            {
                description => "Hyprland's `flake.lock` after substituting transitive inputs with follows.",
                input_file => HYPRLAND_LOCK_NO_FOLLOWS,
                omit_expression => true,
                snapshot_path => "../tests/snapshots",
            },
            {
                assert_json_snapshot!(&lock);
            }
        );
    }

    #[test]
    fn config_hyprland_flake_lock() {
        use crate::print_flake_follows_config;
        let lock = read_flake_lock(HYPRLAND_LOCK_NO_FOLLOWS.into());
        let mut buf = Vec::new();
        print_flake_follows_config(&lock, &mut buf);
        let output = String::from_utf8(buf).unwrap();
        insta::with_settings!(
            {
                description => "Generated config for Hyprland's `flake.lock`.",
                input_file => HYPRLAND_LOCK_NO_FOLLOWS,
                omit_expression => true,
                snapshot_path => "../tests/snapshots",
            },
            {
                insta::assert_snapshot!(output);
            }
        );
    }

    #[test]
    fn config_in_place_test() {
        let temp_dir = std::env::temp_dir().join("allfollow_test_in_place");
        if temp_dir.exists() {
            fs::remove_dir_all(&temp_dir).unwrap();
        }
        fs::create_dir_all(&temp_dir).unwrap();

        let lock_src = "samples/hyprland/no-follows/flake.lock";
        let lock_dest = temp_dir.join("flake.lock");
        fs::copy(lock_src, &lock_dest).unwrap();

        let flake_nix_path = temp_dir.join("flake.nix");
        let initial_content = r#"
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  # START INPUT FOLLOW BLOCK -- DO NOT EDIT MANUALLY
  # END INPUT FOLLOW BLOCK -- DO NOT EDIT MANUALLY

  outputs = { self, nixpkgs }: { };
}
"#;
        fs::write(&flake_nix_path, initial_content).unwrap();

        // Simulate CLI run
        // We can't call main() directly easily because it parses env args.
        // We can call the logic inside Config command.

        let lock = read_flake_lock(Input::File(lock_dest.clone()));
        let mut buf = Vec::new();
        print_flake_follows_config(&lock, &mut buf);
        let config_output = String::from_utf8(buf).unwrap();

        update_flake_nix(&flake_nix_path, &config_output);

        let updated_content = fs::read_to_string(&flake_nix_path).unwrap();

        assert!(updated_content.contains("inputs = {"));
        assert!(updated_content.contains("aquamarine.inputs.hyprutils.follows = \"hyprutils\";"));
        assert!(updated_content.contains("# START INPUT FOLLOW BLOCK -- DO NOT EDIT MANUALLY"));

        // Verify indentation (should be 2 spaces based on initial content)
        // Note: print_flake_follows_config adds its own newlines, and we join with indentation.
        // The block should start with 2 spaces.
        assert!(updated_content.contains("\n  inputs = {"));
    }
}
