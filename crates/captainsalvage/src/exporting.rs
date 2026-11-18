//! Exporting one file or folder, and copying a Disk Rebuild.
//!
//! Both reuse the copy engine in `export.rs`: it reads from the disk being examined, never
//! overwrites a file, hashes each copy as it is written, and writes the manifest the copy
//! report reads. This module only says what to copy and where it lands.

use crate::export::{canvas_target, CopyPlan, CopyRequest};
use crate::session::Session;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

pub const KIND_FILE: &str = "file";
pub const KIND_FOLDER: &str = "folder";

fn include_partial_by_default() -> bool {
    true
}

/// What an export is told, beside the file or folder.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExportOptions {
    pub destination: String,
    pub min_confidence: f64,
    /// The person chose this file, so a damaged one is copied too and the report says so.
    pub include_partial: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            destination: String::new(),
            min_confidence: 0.0,
            include_partial: include_partial_by_default(),
        }
    }
}

/// What a Disk Rebuild copy is told.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RebuildCopyOptions {
    pub destination: String,
    pub min_confidence: f64,
    pub include_partial: bool,
}

impl Session {
    /// The copy an export of one file or folder makes. `kind` is `file` or `folder`;
    /// `entry` is accepted for `file`.
    pub fn export_request(
        &self,
        kind: &str,
        target: i64,
        options: &ExportOptions,
    ) -> Result<CopyRequest> {
        let mut request = CopyRequest {
            destination: options.destination.clone(),
            min_confidence: options.min_confidence,
            include_partial: options.include_partial,
            direct: true,
            ..CopyRequest::default()
        };
        match kind {
            KIND_FILE | "entry" => {
                let row = self
                    .with_database(|database| database.entry(target))?
                    .with_context(|| format!("there is no file {target}"))?;
                if row.kind != "file" {
                    bail!("That is a folder. Export it as a folder.");
                }
                request.entry_ids = vec![target];
            }
            KIND_FOLDER => {
                let head = u32::try_from(target).context("there is no such folder")?;
                if self
                    .with_database(|database| database.node(head))?
                    .is_none()
                {
                    bail!("there is no folder at cluster {target}");
                }
                request.folders = vec![head];
            }
            other => bail!("kind must be {KIND_FILE} or {KIND_FOLDER}, not {other}"),
        }
        Ok(request)
    }

    /// What exporting a file or folder would do, without writing.
    pub fn export_plan(
        &self,
        kind: &str,
        target: i64,
        options: &ExportOptions,
    ) -> Result<CopyPlan> {
        let request = self.export_request(kind, target, options)?;
        self.copy_plan(&request)
    }

    /// Copies a file or folder to the destination, with the live status and the copy report.
    pub fn export_start(
        self: &Arc<Self>,
        kind: &str,
        target: i64,
        options: &ExportOptions,
    ) -> Result<CopyPlan> {
        let request = self.export_request(kind, target, options)?;
        self.copy_start(request)
    }

    /// Where a Disk Rebuild copies to: a folder named after it, inside the destination.
    fn rebuild_request(&self, rebuild: i64, options: &RebuildCopyOptions) -> Result<CopyRequest> {
        let name = self
            .with_database(|database| database.canvases())?
            .into_iter()
            .find(|found| found.id == rebuild)
            .map(|found| found.name)
            .with_context(|| format!("there is no Disk Rebuild {rebuild}"))?;
        let base = if options.destination.trim().is_empty() {
            self.salvage_drive()?.unwrap_or_default()
        } else {
            options.destination.clone()
        };
        // Left empty, the plan refuses it and says to choose a destination.
        let destination = if base.trim().is_empty() {
            String::new()
        } else {
            Path::new(&base)
                .join(canvas_target(&[], &name))
                .display()
                .to_string()
        };
        Ok(CopyRequest {
            destination,
            min_confidence: options.min_confidence,
            include_partial: options.include_partial,
            canvas: Some(rebuild),
            ..CopyRequest::default()
        })
    }

    /// What copying a Disk Rebuild's tree would do, without writing.
    pub fn disk_rebuild_copy_plan(
        &self,
        rebuild: i64,
        options: &RebuildCopyOptions,
    ) -> Result<CopyPlan> {
        let request = self.rebuild_request(rebuild, options)?;
        self.copy_plan(&request)
    }

    /// Copies a Disk Rebuild's tree, each file at its rebuilt path.
    pub fn disk_rebuild_copy_start(
        self: &Arc<Self>,
        rebuild: i64,
        options: &RebuildCopyOptions,
    ) -> Result<CopyPlan> {
        let request = self.rebuild_request(rebuild, options)?;
        self.copy_start(request)
    }
}
