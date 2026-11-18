//! The tools that build and change a Disk Rebuild, shared by MCP, the window and the command line.
//!
//! An agent may build a Disk Rebuild for the person. A Disk Rebuild is virtual: these tools write
//! only the app's own database on this Mac, never a source disk, and every change an agent makes
//! is journaled so the person can see and undo it (see `disk_rebuild_agent`). What an agent may
//! not do is start a copy: that, and the person's own review of what an agent did, are in
//! [`PERSON_ONLY_TOOLS`], and the engine refuses them from any agent before it reads their
//! arguments.

use crate::catalog_tools::Definition;
use crate::engine::{
    bool_of, need_i64, need_rebuild, need_str, need_u32, path_of_names, place_of, str_of, u64_of,
};
use crate::map::MapDb;
use anyhow::{bail, Result};
use serde_json::{json, Value};

/// The tools an agent may use to change a Disk Rebuild. Each is journaled as an agent change.
pub const AGENT_CHANGING_TOOLS: &[&str] = &[
    "disk_rebuild_create",
    "disk_rebuild_rename",
    "disk_rebuild_delete",
    "disk_rebuild_duplicate",
    "disk_rebuild_folder",
    "disk_rebuild_rename_item",
    "disk_rebuild_move",
    "disk_rebuild_remove",
    "disk_rebuild_place",
    "disk_rebuild_place_file",
    "disk_rebuild_swap",
    "disk_rebuild_leftovers",
    "disk_rebuild_choose",
];

/// The tools that answer about a Disk Rebuild and change nothing. An agent may use all of them.
pub const READING_TOOLS: &[&str] = &[
    "disk_rebuild_list",
    "disk_rebuild_items",
    "disk_rebuild_tree",
    "disk_rebuild_resolve",
    "disk_rebuild_clashes",
    "disk_rebuild_unused",
    "disk_rebuild_suggestions",
    "disk_rebuild_copy_plan",
    "disk_rebuild_agent_changes",
];

/// What the engine refuses every agent, whatever its arguments: starting a copy to a destination,
/// stopping one, choosing where recovered files go, and the person's own answers to what an
/// agent did (accepting a suggestion, and undoing or restoring agent changes). None of these is
/// listed to agents.
pub const PERSON_ONLY_TOOLS: &[&str] = &[
    "export_start",
    "disk_rebuild_copy_start",
    "zip_member_copy",
    "copy_stop",
    "set_salvage_drive",
    "disk_rebuild_accept",
    "disk_rebuild_dismiss",
    "disk_rebuild_undo",
    "disk_rebuild_undo_session",
    "disk_rebuild_restore",
];

/// The tools that start copying files out of a source, none of which an agent may run.
pub const COPY_STARTING_TOOLS: &[&str] =
    &["export_start", "disk_rebuild_copy_start", "zip_member_copy"];

/// Why an agent was refused a person-only tool, in words it can pass on. None for other tools.
pub fn person_only_reason(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "export_start" => "Only the person using the app can start an export. Use export_plan, and ask them to start it.",
        "disk_rebuild_copy_start" => "Only the person using the app can start copying. Use disk_rebuild_copy_plan, and ask them to start it.",
        "zip_member_copy" => "Only the person using the app copies files out. Tell them which file, and its size.",
        "copy_stop" => "Only the person using the app can stop a copy.",
        "set_salvage_drive" => "Only the person using the app can set the salvage drive. Suggest one, and ask them to set it.",
        "disk_rebuild_accept" | "disk_rebuild_dismiss" => "Only the person using the app accepts or dismisses a suggestion. Change the Disk Rebuild yourself if they asked you to build it.",
        "disk_rebuild_undo" | "disk_rebuild_undo_session" | "disk_rebuild_restore" => "Only the person using the app undoes agent changes or restores a Disk Rebuild. Say what you changed, and they can undo it.",
        _ => return None,
    })
}

fn integer(description: &str) -> Value {
    json!({"type": "integer", "description": description})
}

fn string(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn rebuild() -> Value {
    integer("Disk Rebuild id, from disk_rebuild_list.")
}

fn place_property() -> Value {
    json!({"type": "object", "description": "Where it goes: item (a Disk Rebuild item id, from disk_rebuild_items) and optionally head (a found folder inside that item's piece). Leave out for the first thing placed, which becomes the top of the tree.", "properties": {"item": integer("Disk Rebuild item id."), "head": integer("Found folder inside that item.")}})
}

fn path_property() -> Value {
    json!({"type": "array", "items": {"type": "string"}, "description": "Names from the top of the rebuilt tree down, ending with the file's name. Folders whose name was lost are `lost:<cluster>`."})
}

/// The tools in this module as MCP lists them, before the name gets its prefix. Only what an agent
/// may use is here: the person-only tools are never listed.
pub fn definitions() -> Vec<Definition> {
    vec![
        Definition {
            name: "disk_rebuild_create",
            description: "Makes a new, empty Disk Rebuild on the open scan, with a name. A Disk Rebuild is a virtual tree built from the segments the scan found: found folders and files laid over one another, as layers. Build one only when the person asked you to. Then put a first piece or a folder in it. This changes only the app's own database on this Mac, never the source. The change shows at once, marked as made by the agent, and the person can undo it. A call may change at most 1000 rows and an agent session at most 10000.",
            properties: json!({"name": string("The Disk Rebuild's name.")}),
            required: &["name"],
        },
        Definition {
            name: "disk_rebuild_rename",
            description: "Renames a Disk Rebuild. It changes only the app's own database, and the person can undo it.",
            properties: json!({"diskRebuild": rebuild(), "name": string("The new name.")}),
            required: &["diskRebuild", "name"],
        },
        Definition {
            name: "disk_rebuild_delete",
            description: "Deletes a Disk Rebuild and everything placed in it. The pieces it used are free again. The person can undo this, and the scan and the source are untouched. Prefer disk_rebuild_duplicate and changing the copy to deleting work the person made.",
            properties: json!({"diskRebuild": rebuild()}),
            required: &["diskRebuild"],
        },
        Definition {
            name: "disk_rebuild_duplicate",
            description: "Copies a Disk Rebuild under a new name: the same items in the same places, and the same chosen copies. Use it to try another arrangement without losing the first. The copy is independent.",
            properties: json!({"diskRebuild": rebuild(), "name": string("The copy's name.")}),
            required: &["diskRebuild", "name"],
        },
        Definition {
            name: "disk_rebuild_folder",
            description: "Makes a folder the disk lost inside a Disk Rebuild, so pieces can go in it. Without a place it is the top of the tree, which only the first thing placed can be. This changes only the app's own database, and the person can undo it.",
            properties: json!({"diskRebuild": rebuild(), "name": string("The folder's name."), "place": place_property()}),
            required: &["diskRebuild", "name"],
        },
        Definition {
            name: "disk_rebuild_rename_item",
            description: "Renames an item of a Disk Rebuild: a folder made up, or a found folder placed (the new name replaces its own). Leave name out to put back a placed folder's own name.",
            properties: json!({"diskRebuild": rebuild(), "item": integer("Disk Rebuild item id."), "name": string("The new name.")}),
            required: &["diskRebuild", "item"],
        },
        Definition {
            name: "disk_rebuild_move",
            description: "Moves an item of a Disk Rebuild, and everything in it, somewhere else in the same Disk Rebuild, as a folder of its own or merged into the folder it lands in. The top of the tree cannot move, and an item cannot go inside itself.",
            properties: json!({"diskRebuild": rebuild(), "item": integer("Disk Rebuild item id."), "place": place_property(), "merge": {"type": "boolean", "description": "Join its contents into that folder instead of keeping it as a folder of its own."}}),
            required: &["diskRebuild", "item", "place"],
        },
        Definition {
            name: "disk_rebuild_remove",
            description: "Takes an item off a Disk Rebuild with everything in it. The pieces it used are free again, and the scan and the source are untouched. The person can undo it.",
            properties: json!({"diskRebuild": rebuild(), "item": integer("Disk Rebuild item id.")}),
            required: &["diskRebuild", "item"],
        },
        Definition {
            name: "disk_rebuild_place",
            description: "Adds a segment to a Disk Rebuild: a found folder, from disk_rebuild_unused or any found folder, as a folder of its own or merged into the folder it lands in. A merge puts the folder's files and folders into that folder, and where the same file name is held twice the layers clash: the later item is the upper layer. A found folder can be used once per Disk Rebuild. The first one placed becomes the top of the tree. This changes only the app's own database, and the person can undo it.",
            properties: json!({"diskRebuild": rebuild(), "head": integer("First cluster of the found folder."), "place": place_property(), "merge": {"type": "boolean", "description": "Join its contents into that folder rather than placing it as a folder of its own."}}),
            required: &["diskRebuild", "head"],
        },
        Definition {
            name: "disk_rebuild_place_file",
            description: "Adds one found file to a Disk Rebuild, as a small layer of its own, inside a folder of the tree. It clashes with a file of the same name there like any other copy. The first thing placed cannot be a file.",
            properties: json!({"diskRebuild": rebuild(), "entry": integer("File entry id, from search or children."), "place": place_property()}),
            required: &["diskRebuild", "entry", "place"],
        },
        Definition {
            name: "disk_rebuild_swap",
            description: "Swaps the found folder behind a placed item for another unused one, keeping where it sits. For trading a piece for a similar one with better data (see similar_pieces). Refused when something is placed inside a found folder of the old one.",
            properties: json!({"diskRebuild": rebuild(), "item": integer("Disk Rebuild item id."), "head": integer("First cluster of the unused found folder.")}),
            required: &["diskRebuild", "item", "head"],
        },
        Definition {
            name: "disk_rebuild_leftovers",
            description: "Puts every unused piece, or only those with at most maxFiles files, into one made-up folder inside the top of the tree, so the small ones need not be placed one by one. It may change many rows, and fails with a clear error naming the limit when it would change too many.",
            properties: json!({"diskRebuild": rebuild(), "name": string("The folder's name."), "maxFiles": integer("Only pieces with at most this many files.")}),
            required: &["diskRebuild", "name"],
        },
        Definition {
            name: "disk_rebuild_choose",
            description: "Chooses which layer's copy of a file the rebuilt tree shows, where several layers hold the same file name. Give the copy's entry id, or the layer (the item id of the layer it came from, as disk_rebuild_resolve lists them), which lets a lower layer win a path an upper one would take. Leave both out to go back to the best copy. Read disk_rebuild_resolve first to see the copies and why one wins.",
            properties: json!({"diskRebuild": rebuild(), "path": path_property(), "entry": integer("Entry id of the copy to show."), "layer": integer("Item id of the layer whose copy to show.")}),
            required: &["diskRebuild", "path"],
        },
        Definition {
            name: "disk_rebuild_resolve",
            description: "What one path of a Disk Rebuild's rebuilt tree resolves to. For a file: the copy that fills it, every copy with the layer (item) it came from, which layer won and why (the person's or an agent's choice, a better measure, the newer date, or the upper layer), and whether the copies differ but measure the same, so the person should look. For a folder: what it is made of. Use it to read a tree back and before choosing a layer.",
            properties: json!({"diskRebuild": rebuild(), "path": path_property()}),
            required: &["diskRebuild", "path"],
        },
        Definition {
            name: "disk_rebuild_agent_changes",
            description: "What agents have changed on this scan's Disk Rebuilds, newest first, with the agent sessions that made them and how much of the session limit each has used. The person sees the same list and can undo any of it. Use it to say what you changed. Undoing is the person's.",
            properties: json!({"diskRebuild": integer("Only changes to this Disk Rebuild."), "session": string("Only this agent session."), "includeUndone": {"type": "boolean", "description": "Also list changes already undone."}, "limit": integer("Default 50.")}),
            required: &[],
        },
    ]
}

/// A short sentence naming what a change did, for the journal.
pub fn summarize(tool: &str, args: &Value) -> String {
    let name = str_of(args, "name").unwrap_or("");
    let item = args.get("item").and_then(Value::as_i64);
    match tool {
        "disk_rebuild_create" => format!("Made the Disk Rebuild \"{name}\""),
        "disk_rebuild_rename" => format!("Renamed the Disk Rebuild to \"{name}\""),
        "disk_rebuild_delete" => "Deleted the Disk Rebuild".to_string(),
        "disk_rebuild_duplicate" => format!("Duplicated the Disk Rebuild as \"{name}\""),
        "disk_rebuild_folder" => format!("Made the folder \"{name}\""),
        "disk_rebuild_rename_item" => match str_of(args, "name") {
            Some(name) => format!("Renamed item {} to \"{name}\"", item.unwrap_or_default()),
            None => format!("Put back the own name of item {}", item.unwrap_or_default()),
        },
        "disk_rebuild_move" => format!("Moved item {}", item.unwrap_or_default()),
        "disk_rebuild_remove" => format!("Took item {} off", item.unwrap_or_default()),
        "disk_rebuild_place" => format!(
            "{} the folder at cluster {}",
            if bool_of(args, "merge", false) {
                "Merged"
            } else {
                "Placed"
            },
            u64_of(args, "head").unwrap_or_default()
        ),
        "disk_rebuild_place_file" => format!(
            "Placed the file {}",
            args.get("entry")
                .and_then(Value::as_i64)
                .unwrap_or_default()
        ),
        "disk_rebuild_swap" => format!(
            "Swapped item {} for the folder at cluster {}",
            item.unwrap_or_default(),
            u64_of(args, "head").unwrap_or_default()
        ),
        "disk_rebuild_leftovers" => format!("Put the unused pieces away in \"{name}\""),
        "disk_rebuild_choose" => {
            let path = path_of_names(args).unwrap_or_default().join("/");
            if let Some(layer) = args.get("layer").and_then(Value::as_i64) {
                format!("Chose the copy of {path} from layer {layer}")
            } else if args.get("entry").is_some_and(|entry| !entry.is_null()) {
                format!("Chose a copy of {path}")
            } else {
                format!("Went back to the best copy of {path}")
            }
        }
        other => other.to_string(),
    }
}

/// The Disk Rebuild a change tool acts on, or none when it makes a new one (a copy is a new one).
pub fn target_of(tool: &str, args: &Value) -> Result<Option<i64>> {
    if matches!(tool, "disk_rebuild_create" | "disk_rebuild_duplicate") {
        Ok(None)
    } else {
        Ok(Some(need_rebuild(args)?))
    }
}

/// Does what a change tool says, and tells the caller the Disk Rebuild it changed. It does not
/// check who asked: the engine does that, and journals what an agent does.
pub fn apply(db: &MapDb, tool: &str, args: &Value) -> Result<(Value, i64)> {
    if tool == "disk_rebuild_create" {
        let id = db.disk_rebuild_create(need_str(args, "name")?)?;
        return Ok((json!({"id": id}), id));
    }
    let rebuild = need_rebuild(args)?;
    let value = match tool {
        "disk_rebuild_rename" => {
            db.disk_rebuild_rename(rebuild, need_str(args, "name")?)?;
            json!({"ok": true})
        }
        "disk_rebuild_delete" => {
            db.disk_rebuild_delete(rebuild)?;
            json!({"ok": true})
        }
        "disk_rebuild_duplicate" => {
            let id = db.disk_rebuild_duplicate(rebuild, need_str(args, "name")?)?;
            return Ok((json!({"id": id}), id));
        }
        "disk_rebuild_place" => json!({"id": db.disk_rebuild_place(
            rebuild,
            need_u32(args, "head")?,
            place_of(args)?,
            bool_of(args, "merge", false),
        )?}),
        "disk_rebuild_place_file" => json!({"id": db.disk_rebuild_place_file(
            rebuild,
            need_i64(args, "entry")?,
            place_of(args)?,
        )?}),
        "disk_rebuild_folder" => json!({"id": db.disk_rebuild_folder(
            rebuild,
            need_str(args, "name")?,
            place_of(args)?,
        )?}),
        "disk_rebuild_move" => {
            db.disk_rebuild_move(
                rebuild,
                need_i64(args, "item")?,
                place_of(args)?,
                args.get("merge").and_then(Value::as_bool),
            )?;
            json!({"ok": true})
        }
        "disk_rebuild_rename_item" => {
            db.disk_rebuild_rename_item(rebuild, need_i64(args, "item")?, str_of(args, "name"))?;
            json!({"ok": true})
        }
        "disk_rebuild_swap" => {
            db.disk_rebuild_swap(rebuild, need_i64(args, "item")?, need_u32(args, "head")?)?;
            json!({"ok": true})
        }
        "disk_rebuild_remove" => {
            json!({"removed": db.disk_rebuild_remove(rebuild, need_i64(args, "item")?)?})
        }
        "disk_rebuild_leftovers" => {
            let (folder, pieces) = db.disk_rebuild_leftovers(
                rebuild,
                need_str(args, "name")?,
                u64_of(args, "maxFiles"),
            )?;
            json!({"folder": folder, "pieces": pieces})
        }
        "disk_rebuild_choose" => {
            let path = path_of_names(args)?;
            match args.get("layer").and_then(Value::as_i64) {
                Some(layer) => db.disk_rebuild_choose_layer(rebuild, &path, layer)?,
                None => db.disk_rebuild_choose(
                    rebuild,
                    &path,
                    args.get("entry").and_then(Value::as_i64),
                )?,
            }
            json!({"ok": true})
        }
        // The person's review of a suggestion is theirs alone: the engine never lets an agent here.
        "disk_rebuild_accept" => {
            json!({"id": db.disk_rebuild_accept(rebuild, need_i64(args, "suggestion")?)?})
        }
        "disk_rebuild_dismiss" => {
            db.disk_rebuild_dismiss(rebuild, need_i64(args, "suggestion")?)?;
            json!({"ok": true})
        }
        other => bail!("unknown tool {other}"),
    };
    Ok((value, rebuild))
}

/// Whether a tool changes a Disk Rebuild, for an agent or for the person.
pub fn changes_a_disk_rebuild(tool: &str) -> bool {
    AGENT_CHANGING_TOOLS.contains(&tool)
        || matches!(tool, "disk_rebuild_accept" | "disk_rebuild_dismiss")
}
