use std::{borrow::Cow, fmt, str::FromStr, sync::Arc};

use objdiff_core::bindings::report::{Measures, Report, ReportCategory, ReportUnit};
use serde::{Deserialize, Serialize};
use time::UtcDateTime;

// BLAKE3 hash of the image data
pub type ImageId = [u8; 32];

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PullReportStyle {
    #[default]
    Comment,
    Description,
}

impl PullReportStyle {
    pub const fn variants() -> &'static [Self] { &[Self::Comment, Self::Description] }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Comment => "comment",
            Self::Description => "description",
        }
    }
}

impl FromStr for PullReportStyle {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "comment" => Ok(Self::Comment),
            "description" => Ok(Self::Description),
            _ => Err(()),
        }
    }
}

impl fmt::Display for PullReportStyle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Comment => "Comment",
            Self::Description => "Description",
        })
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct Project {
    pub id: u64,
    pub owner: String,
    pub repo: String,
    pub name: Option<String>,
    pub short_name: Option<String>,
    pub default_category: Option<String>,
    pub default_version: Option<String>,
    pub platform: Option<String>,
    pub workflow_id: Option<String>,
    pub enable_pr_comments: bool,
    pub pr_report_style: PullReportStyle,
    pub header_image_id: Option<ImageId>,
    pub enabled: bool,
    pub permanently_disabled: bool,
}

impl Default for Project {
    fn default() -> Self {
        Self {
            id: 0,
            owner: String::new(),
            repo: String::new(),
            name: None,
            short_name: None,
            default_category: None,
            default_version: None,
            platform: None,
            workflow_id: None,
            enable_pr_comments: true,
            pr_report_style: PullReportStyle::Comment,
            header_image_id: None,
            enabled: true,
            permanently_disabled: false,
        }
    }
}

impl Project {
    pub fn name(&self) -> Cow<'_, str> {
        if let Some(name) = self.name.as_ref() {
            Cow::Borrowed(name)
        } else {
            Cow::Owned(format!("{}/{}", self.owner, self.repo))
        }
    }

    pub fn short_name(&self) -> &str {
        self.short_name.as_deref().or(self.name.as_deref()).unwrap_or(&self.repo)
    }

    pub fn repo_url(&self) -> String { format!("https://github.com/{}/{}", self.owner, self.repo) }

    pub fn default_category(&self) -> &str { self.default_category.as_deref().unwrap_or("all") }

    pub fn reports_enabled(&self) -> bool { self.enabled && !self.permanently_disabled }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ProjectInfo {
    pub project: Project,
    pub commit: Option<Commit>,
    pub report_versions: Vec<String>,
    pub prev_commit: Option<String>,
    pub next_commit: Option<String>,
}

impl ProjectInfo {
    pub fn default_version(&self) -> Option<&str> {
        self.project
            .default_version
            .as_ref()
            // Verify that the default version is in the list of report versions
            .and_then(|v| self.report_versions.contains(v).then_some(v.as_str()))
            // Otherwise, return the first version in the list
            .or_else(|| self.report_versions.first().map(String::as_str))
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub message: Option<String>,
    pub timestamp: UtcDateTime,
}

#[derive(Debug, Clone)]
pub struct ReportFile<R> {
    pub commit: Commit,
    pub version: String,
    pub report: R,
}

pub type CachedReportFile = ReportFile<Arc<CachedReport>>;
pub type FullReportFile = ReportFile<FullReport>;

#[derive(Debug, Clone)]
pub struct ReportInner<U> {
    pub version: u32,
    pub measures: Measures,
    pub units: Vec<U>,
    pub categories: Vec<ReportCategory>,
}

// BLAKE3 hash of the unit data
pub type UnitKey = [u8; 32];

pub type CachedReport = ReportInner<UnitKey>;
pub type FullReport = ReportInner<Arc<ReportUnit>>;

impl<U> ReportInner<U> {
    /// Fetch the measures for a specific category (None for the "all" category)
    pub fn measures(&self, category: Option<&str>) -> &Measures {
        if let Some(category) = category {
            self.categories
                .iter()
                .find(|c| c.id == category)
                .and_then(|c| c.measures.as_ref())
                .unwrap_or(&self.measures)
        } else {
            &self.measures
        }
    }
}

impl FullReport {
    /// Flatten the report into the standard format
    pub fn flatten(&self) -> Report {
        let mut units = Vec::with_capacity(self.units.len());
        for unit in &self.units {
            units.push((**unit).clone());
        }
        Report {
            measures: Some(self.measures),
            units,
            version: self.version,
            categories: self.categories.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FrogressMapping {
    pub frogress_slug: String,
    pub frogress_version: String,
    pub frogress_category: String,
    pub frogress_measure: String,
    pub project_id: u64,
    pub project_version: String,
    pub project_category: String,
    pub project_category_name: String,
    pub project_measure: String,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Platform {
    PS,      // 1994
    Win32,   // 1995
    N64,     // 1996
    PS2,     // 2000
    GBA,     // 2001
    GC,      // 2001
    Xbox,    // 2001
    DS,      // 2004
    PSP,     // 2004
    Xbox360, // 2005
    Wii,     // 2006
    _3DS,    // 2011
    Switch,  // 2017
}

pub const ALL_PLATFORMS: &[Platform] = &[
    Platform::PS,
    Platform::Win32,
    Platform::N64,
    Platform::PS2,
    Platform::GBA,
    Platform::GC,
    Platform::Xbox,
    Platform::DS,
    Platform::PSP,
    Platform::Xbox360,
    Platform::Wii,
    Platform::_3DS,
    Platform::Switch,
];

impl Platform {
    pub fn to_str(self) -> &'static str {
        match self {
            Self::PS => "ps",
            Self::Win32 => "win32",
            Self::N64 => "n64",
            Self::PS2 => "ps2",
            Self::GBA => "gba",
            Self::GC => "gc",
            Self::Xbox => "xbox",
            Self::DS => "nds",
            Self::PSP => "psp",
            Self::Xbox360 => "xbox360",
            Self::Wii => "wii",
            Self::_3DS => "3ds",
            Self::Switch => "switch",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Platform::PS => "PlayStation",
            Platform::Win32 => "Windows",
            Platform::N64 => "Nintendo 64",
            Platform::PS2 => "PlayStation 2",
            Platform::GBA => "Game Boy Advance",
            Platform::GC => "GameCube",
            Platform::Xbox => "Xbox",
            Platform::DS => "Nintendo DS",
            Platform::PSP => "PlayStation Portable",
            Platform::Xbox360 => "Xbox 360",
            Platform::Wii => "Wii",
            Platform::_3DS => "Nintendo 3DS",
            Platform::Switch => "Switch",
        }
    }
}

impl FromStr for Platform {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ps" => Ok(Self::PS),
            "win32" => Ok(Self::Win32),
            "n64" => Ok(Self::N64),
            "ps2" => Ok(Self::PS2),
            "gba" => Ok(Self::GBA),
            "gc" => Ok(Self::GC),
            "xbox" => Ok(Self::Xbox),
            "nds" => Ok(Self::DS),
            "psp" => Ok(Self::PSP),
            "xbox360" => Ok(Self::Xbox360),
            "wii" => Ok(Self::Wii),
            "3ds" => Ok(Self::_3DS),
            "switch" => Ok(Self::Switch),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectVisibility {
    Visible,
    Hidden,
    Disabled,
}

pub fn project_visibility(project: &Project, measures: Option<&Measures>) -> ProjectVisibility {
    // Hide projects with less than 0.5% matched code or if the project is disabled
    if !project.reports_enabled() {
        ProjectVisibility::Disabled
    } else if measures.is_none_or(|m| m.matched_code_percent < 0.5) {
        ProjectVisibility::Hidden
    } else {
        ProjectVisibility::Visible
    }
}
