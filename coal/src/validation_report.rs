//! Gathering what a validation run found into one report.
//!
//! A configuration that declares a `_structure.yaml` is not one configuration
//! but many, and validating it means compiling and checking every one of them.
//! That makes most findings arrive several times over: the same line of the
//! same file is read once per selection, so one mistake in a shared file is
//! reported once per node, per mode, per environment it reaches.
//!
//! Reporting them as they arrive would bury the one thing worth knowing — that
//! there is a mistake, and where — under a list of the configurations that ran
//! into it. So a finding is reported once, at the place it is written, and
//! says how far it reaches:
//!
//! * **in all 12 configurations** — a mistake in something everything shares.
//! * **in 3 of 12 configurations, whenever mode = test** — a mistake the
//!   selections reach into, which names the selection responsible for it.
//! * **in 1 of 12 configurations: cop02, test** — a mistake that follows no
//!   selection in particular, which names the configurations instead.
//!
//! The last of those is what a report falls back to. A finding that appears
//! under an odd handful of combinations has no shorter honest description than
//! the handful itself.

use crate::coal_tree::Span;
use crate::diagnostics::DiagnosticError;
use crate::tree_builder::CompiledConfiguration;
use std::collections::BTreeSet;

/// How many configurations a finding names one by one before it gives up and
/// counts them instead. Past this the list stops being something to read.
const MAX_LISTED_CONFIGURATIONS: usize = 6;

/// One mistake, wherever it was found and however many configurations found
/// it.
#[derive(Debug, Clone)]
pub struct GroupedFinding {
    pub error_type: String,
    /// The file it is written in, exactly as the build reported it, so a
    /// language server can still map it to a document.
    pub file_origin: String,
    pub span: Option<Span>,
    /// What is wrong, with nothing in it that changes from one selection to
    /// the next.
    pub detail: String,
    /// Anything the first report of it added below the first line — the fields
    /// an unresolved template is holding up, say.
    pub elaboration: String,
    /// The field paths it was found on, in the order they were found.
    pub field_paths: Vec<String>,
    /// Which of the configurations ran into it, by the names they were
    /// compiled under.
    pub affected: Vec<String>,
    /// How many configurations were checked in total.
    pub total_configurations: usize,
    /// The selections that decide it: every affected configuration makes them
    /// and every unaffected one does not. Empty when the finding follows no
    /// selection in particular, and when everything ran into it.
    pub responsible_selections: Vec<(String, String)>,
}

impl GroupedFinding {
    /// Whether every configuration checked ran into this.
    pub fn reaches_everything(&self) -> bool {
        self.affected.len() == self.total_configurations
    }

    /// `file:line:col`, as an editor and a person both read it.
    pub fn location(&self) -> String {
        match &self.span {
            Some(span) => format!("{}:{}:{}", self.file_origin, span.line, span.col),
            None => self.file_origin.clone(),
        }
    }

    /// The field the finding is about. Several selections reach the same line
    /// under different paths, and naming the first with a count says that
    /// without listing them.
    pub fn describe_fields(&self) -> String {
        match self.field_paths.split_first() {
            None => String::new(),
            Some((first, [])) => first.clone(),
            Some((first, rest)) => format!("{} (and {} more)", first, rest.len()),
        }
    }

    /// How far the finding reaches, or nothing at all when there was only ever
    /// one configuration to reach.
    pub fn describe_reach(&self) -> Option<String> {
        if self.total_configurations <= 1 {
            return None;
        }

        if self.reaches_everything() {
            return Some(format!(
                "in all {} configurations",
                self.total_configurations
            ));
        }

        let counted = format!(
            "in {} of {} configurations",
            self.affected.len(),
            self.total_configurations
        );

        if !self.responsible_selections.is_empty() {
            let named: Vec<String> = self
                .responsible_selections
                .iter()
                .map(|(name, value)| format!("{} = {}", name, value))
                .collect();
            return Some(format!("{}, whenever {}", counted, named.join(" and ")));
        }

        if self.affected.len() <= MAX_LISTED_CONFIGURATIONS {
            return Some(format!("{}: {}", counted, self.affected.join("; ")));
        }

        let listed = self.affected[..MAX_LISTED_CONFIGURATIONS].join("; ");
        Some(format!(
            "{}: {}; and {} more",
            counted,
            listed,
            self.affected.len() - MAX_LISTED_CONFIGURATIONS
        ))
    }
}

/// Gathers every finding from every configuration that was checked.
///
/// Findings keep the order of the file they are written in, so a report reads
/// the way the configuration does rather than the way the build happened to
/// walk it.
pub fn group_findings(configurations: &[CompiledConfiguration]) -> Vec<GroupedFinding> {
    let names: Vec<String> = configurations
        .iter()
        .map(CompiledConfiguration::describe_selection)
        .collect();

    let mut grouped: Vec<GroupedFinding> = Vec::new();
    let mut keys: Vec<(String, String, usize, usize, String)> = Vec::new();

    for (index, configuration) in configurations.iter().enumerate() {
        for diagnostic in &configuration.diagnostics {
            let key = diagnostic.grouping_key();
            match keys.iter().position(|existing| existing == &key) {
                Some(position) => add_occurrence(&mut grouped[position], diagnostic, &names[index]),
                None => {
                    keys.push(key);
                    grouped.push(new_finding(diagnostic, &names[index], configurations.len()));
                }
            }
        }
    }

    for finding in &mut grouped {
        finding.responsible_selections = selections_responsible_for(finding, configurations);
    }

    grouped.sort_by(|left, right| {
        let (left_line, left_column) = line_and_column(&left.span);
        let (right_line, right_column) = line_and_column(&right.span);
        left.file_origin
            .cmp(&right.file_origin)
            .then(left_line.cmp(&right_line))
            .then(left_column.cmp(&right_column))
            .then(left.error_type.cmp(&right.error_type))
            .then(left.detail.cmp(&right.detail))
    });

    grouped
}

fn line_and_column(span: &Option<Span>) -> (usize, usize) {
    match span {
        Some(span) => (span.line, span.col),
        None => (0, 0),
    }
}

fn new_finding(
    diagnostic: &DiagnosticError,
    configuration_name: &str,
    total_configurations: usize,
) -> GroupedFinding {
    GroupedFinding {
        error_type: diagnostic.error_type.clone(),
        file_origin: diagnostic.file_origin.clone(),
        span: diagnostic.span.clone(),
        detail: diagnostic.detail.clone(),
        elaboration: elaboration_of(diagnostic),
        field_paths: match diagnostic.field_path.is_empty() {
            true => Vec::new(),
            false => vec![diagnostic.field_path.clone()],
        },
        affected: vec![configuration_name.to_string()],
        total_configurations,
        responsible_selections: Vec::new(),
    }
}

fn add_occurrence(
    finding: &mut GroupedFinding,
    diagnostic: &DiagnosticError,
    configuration_name: &str,
) {
    if !diagnostic.field_path.is_empty() && !finding.field_paths.contains(&diagnostic.field_path) {
        finding.field_paths.push(diagnostic.field_path.clone());
    }
    if !finding.affected.iter().any(|name| name == configuration_name) {
        finding.affected.push(configuration_name.to_string());
    }
}

/// Whatever a finding said below its first line. The first line is rebuilt
/// from the parts, but a finding that goes on to list what it holds up has
/// more to say than the parts carry.
fn elaboration_of(diagnostic: &DiagnosticError) -> String {
    match diagnostic.message.split_once('\n') {
        Some((_, remainder)) => remainder.to_string(),
        None => String::new(),
    }
}

/// The selections that decide a finding: those every affected configuration
/// makes and no unaffected one does.
///
/// The set is then cut back to the smallest that still picks out exactly the
/// affected configurations, because a selection that changes nothing about who
/// is affected explains nothing about why.
fn selections_responsible_for(
    finding: &GroupedFinding,
    configurations: &[CompiledConfiguration],
) -> Vec<(String, String)> {
    if finding.reaches_everything() || configurations.len() <= 1 {
        return Vec::new();
    }

    let affected: BTreeSet<&str> = finding.affected.iter().map(String::as_str).collect();
    let is_affected: Vec<bool> = configurations
        .iter()
        .map(|configuration| affected.contains(configuration.describe_selection().as_str()))
        .collect();

    let mut shared: Vec<(String, String)> = Vec::new();
    for (index, configuration) in configurations.iter().enumerate() {
        if !is_affected[index] {
            continue;
        }
        let Some(combination) = &configuration.selection else {
            return Vec::new();
        };
        match shared.is_empty() {
            true => shared = combination.values.clone(),
            false => shared.retain(|pair| combination.values.contains(pair)),
        }
    }

    if shared.is_empty() || !picks_out_exactly(&shared, configurations, &is_affected) {
        return Vec::new();
    }

    // Drop every selection the description does not need.
    let mut index = 0;
    while index < shared.len() {
        let mut without = shared.clone();
        without.remove(index);
        if !without.is_empty() && picks_out_exactly(&without, configurations, &is_affected) {
            shared = without;
        } else {
            index += 1;
        }
    }

    shared
}

/// Whether making exactly these selections describes exactly the affected
/// configurations — no more and no fewer.
fn picks_out_exactly(
    selections: &[(String, String)],
    configurations: &[CompiledConfiguration],
    is_affected: &[bool],
) -> bool {
    configurations
        .iter()
        .enumerate()
        .all(|(index, configuration)| {
            let matches = configuration.selection.as_ref().is_some_and(|combination| {
                selections
                    .iter()
                    .all(|pair| combination.values.contains(pair))
            });
            matches == is_affected[index]
        })
}
