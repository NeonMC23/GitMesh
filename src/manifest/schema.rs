//! On-disk schema of the GitMesh manifest.
//!
//! These types are only about the TOML representation. Everything semantic (path
//! normalisation, validation, defaults) happens in [`super::ManifestFile::into_project`]
//! so that the file format can stay a dumb data description.

use serde::{Deserialize, Serialize};

/// Root of the manifest file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Manifest format version.
    pub version: u32,
    /// Logical project name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Root repository configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<ManifestRoot>,
    /// External repositories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<ManifestRepository>,
}

/// The repository that owns the project root.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRoot {
    /// Logical id; defaults to `root`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Must be `.` when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Remote URL of the root repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Logical branch used as a default hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// An external physical repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRepository {
    /// Logical identifier, unique within the project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Path relative to the project root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Remote URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Logical branch used as a default hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}
