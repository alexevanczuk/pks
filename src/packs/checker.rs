// Module declarations
pub(crate) mod architecture;
mod dependency;

mod folder_visibility;
mod privacy;
pub(crate) mod reference;
mod visibility;

// Internal imports
use crate::packs::pack::write_pack_to_disk;
use crate::packs::pack::Pack;
use crate::packs::package_todo;
use crate::packs::Configuration;
use crate::packs::PackSet;

use anyhow::bail;
// External imports
use anyhow::Context;
use rayon::prelude::IntoParallelIterator;
use rayon::prelude::IntoParallelRefIterator;
use rayon::prelude::ParallelIterator;
use reference::Reference;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::{collections::HashSet, path::PathBuf};
use tracing::debug;

use super::reference_extractor::get_all_references;

#[derive(PartialEq, Clone, Eq, Hash, Debug)]
pub struct ViolationIdentifier {
    pub violation_type: String,
    pub file: String,
    pub constant_name: String,
    pub referencing_pack_name: String,
    pub defining_pack_name: String,
}

pub fn get_defining_pack<'a>(
    violation: &ViolationIdentifier,
    packset: &'a PackSet,
) -> anyhow::Result<&'a Pack> {
    packset.for_pack(&violation.defining_pack_name)
    .context(format!("ViolationIdentifier#defining_pack is {}, but that pack cannot be found in the packset.", 
    &violation.defining_pack_name))
}

pub fn get_referencing_pack<'a>(
    violation: &ViolationIdentifier,
    packset: &'a PackSet,
) -> anyhow::Result<&'a Pack> {
    packset.for_pack(&violation.referencing_pack_name)
    .context(format!("ViolationIdentifier#referencing_pack is {}, but that pack cannot be found in the packset.",
&violation.referencing_pack_name))
}

#[derive(PartialEq, Clone, Eq, Hash, Debug)]
pub struct Violation {
    message: String,
    pub identifier: ViolationIdentifier,
}

pub(crate) trait CheckerInterface {
    fn check(
        &self,
        reference: &Reference,
        configuration: &Configuration,
    ) -> anyhow::Result<Option<Violation>>;

    fn is_strict_mode_violation(
        &self,
        offense: &ViolationIdentifier,
        configuration: &Configuration,
    ) -> anyhow::Result<bool>;

    fn violation_type(&self) -> String;
}

pub(crate) trait ValidatorInterface {
    fn validate(&self, configuration: &Configuration) -> Option<Vec<String>>;
}

#[derive(Debug, PartialEq)]
pub struct CheckAllResult {
    reportable_violations: HashSet<Violation>,
    stale_violations: Vec<ViolationIdentifier>,
    strict_mode_violations: Vec<ViolationIdentifier>,
}

impl CheckAllResult {
    pub fn has_violations(&self) -> bool {
        !self.reportable_violations.is_empty()
            || !self.stale_violations.is_empty()
            || !self.strict_mode_violations.is_empty()
    }

    fn write_violations(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if !self.reportable_violations.is_empty() {
            writeln!(
                f,
                "{} violation(s) detected:",
                self.reportable_violations.len()
            )?;
            for violation in self.reportable_violations.iter() {
                writeln!(f, "{}", violation.message)?;
            }
        }

        if !self.stale_violations.is_empty() {
            writeln!(
                f,
                "There were stale violations found, please run `packs update`"
            )?;
        }

        if !self.strict_mode_violations.is_empty() {
            for v in self.strict_mode_violations.iter() {
                let error_message = format!("{} cannot have {} violations on {} because strict mode is enabled for {} violations in the enforcing pack's package.yml file",
                                        v.referencing_pack_name,
                                        v.violation_type,
                                        v.defining_pack_name,
                                        v.violation_type
            );
                writeln!(f, "{}", error_message)?;
            }
        }
        Ok(())
    }
}

impl Display for CheckAllResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.has_violations() {
            self.write_violations(f)
        } else {
            write!(f, "No violations detected!")
        }
    }
}
struct CheckAllBuilder<'a> {
    configuration: &'a Configuration,
    found_violations: &'a FoundViolations,
}

struct FoundViolations {
    checkers: Vec<Box<dyn CheckerInterface + Send + Sync>>,
    absolute_paths: HashSet<PathBuf>,
    violations: HashSet<Violation>,
}

impl<'a> CheckAllBuilder<'a> {
    fn new(
        configuration: &'a Configuration,
        found_violations: &'a FoundViolations,
    ) -> Self {
        Self {
            configuration,
            found_violations,
        }
    }

    pub fn build(mut self) -> anyhow::Result<CheckAllResult> {
        let recorded_violations = &self.configuration.pack_set.all_violations;

        Ok(CheckAllResult {
            reportable_violations: self
                .build_reportable_violations(recorded_violations)
                .into_iter()
                .cloned()
                .collect(),
            stale_violations: self
                .build_stale_violations(recorded_violations)
                .into_iter()
                .cloned()
                .collect(),
            strict_mode_violations: self
                .build_strict_mode_violations(recorded_violations)?
                .into_iter()
                .cloned()
                .collect(),
        })
    }

    fn build_reportable_violations(
        &mut self,
        recorded_violations: &HashSet<ViolationIdentifier>,
    ) -> HashSet<&'a Violation> {
        let reportable_violations =
            if self.configuration.ignore_recorded_violations {
                debug!("Filtering recorded violations is disabled in config");
                self.found_violations.violations.iter().collect()
            } else {
                self.found_violations
                    .violations
                    .iter()
                    .filter(|v| !recorded_violations.contains(&v.identifier))
                    .collect()
            };
        reportable_violations
    }

    fn build_stale_violations(
        &mut self,
        recorded_violations: &'a HashSet<ViolationIdentifier>,
    ) -> Vec<&'a ViolationIdentifier> {
        let found_violation_identifiers: HashSet<&ViolationIdentifier> = self
            .found_violations
            .violations
            .par_iter()
            .map(|v| &v.identifier)
            .collect();

        let relative_files = self
            .found_violations
            .absolute_paths
            .iter()
            .map(|p| {
                p.strip_prefix(&self.configuration.absolute_root)
                    .unwrap()
                    .to_str()
                    .unwrap()
            })
            .collect::<HashSet<&str>>();

        let stale_violations = recorded_violations
            .par_iter()
            .filter(|v_identifier| {
                relative_files.contains(&v_identifier.file.as_str())
                    && !found_violation_identifiers.contains(v_identifier)
            })
            .collect::<Vec<&ViolationIdentifier>>();
        stale_violations
    }

    fn build_strict_mode_violations(
        &mut self,
        recorded_violations: &'a HashSet<ViolationIdentifier>,
    ) -> anyhow::Result<Vec<&'a ViolationIdentifier>> {
        let indexed_checkers: HashMap<
            String,
            &Box<dyn CheckerInterface + Send + Sync>,
        > = self
            .found_violations
            .checkers
            .iter()
            .map(|checker| (checker.violation_type(), checker))
            .collect();

        recorded_violations
            .iter()
            .try_fold(vec![], |mut acc, violation| {
                let checker = indexed_checkers
                    .get(&violation.violation_type)
                    .context(format!(
                    "Checker for violation type {} not found",
                    violation.violation_type
                ))?;

                if checker
                    .is_strict_mode_violation(violation, self.configuration)?
                {
                    acc.push(violation);
                }
                Ok(acc)
            })
    }
}

pub(crate) fn check_all(
    configuration: &Configuration,
    files: Vec<String>,
) -> anyhow::Result<CheckAllResult> {
    let checkers = get_checkers(configuration);

    debug!("Intersecting input files with configuration included files");
    let absolute_paths: HashSet<PathBuf> =
        configuration.intersect_files(files.clone());

    let violations: HashSet<Violation> =
        get_all_violations(configuration, &absolute_paths, &checkers)?;
    let found_violations = FoundViolations {
        checkers,
        absolute_paths,
        violations,
    };
    CheckAllBuilder::new(configuration, &found_violations).build()
}

fn validate(configuration: &Configuration) -> Vec<String> {
    debug!("Running validators against packages");
    let validators: Vec<Box<dyn ValidatorInterface + Send + Sync>> = vec![
        Box::new(dependency::Checker {}),
        Box::new(architecture::Checker {
            layers: configuration.layers.clone(),
        }),
    ];

    let mut validation_errors: Vec<String> = validators
        .iter()
        .filter_map(|v| v.validate(configuration))
        .flatten()
        .collect();
    validation_errors.dedup();
    debug!("Finished validators against packages");

    validation_errors
}

pub(crate) fn validate_all(
    configuration: &Configuration,
) -> anyhow::Result<()> {
    let validation_errors = validate(configuration);
    if !validation_errors.is_empty() {
        println!("{} validation error(s) detected:", validation_errors.len());
        for validation_error in validation_errors.iter() {
            println!("{}\n", validation_error);
        }

        bail!("Packwerk validate failed")
    } else {
        println!("Packwerk validate succeeded!");
        Ok(())
    }
}

pub(crate) fn update(configuration: &Configuration) -> anyhow::Result<()> {
    let checkers = get_checkers(configuration);

    let violations = get_all_violations(
        configuration,
        &configuration.included_files,
        &checkers,
    )?;

    package_todo::write_violations_to_disk(configuration, violations);
    println!("Successfully updated package_todo.yml files!");
    Ok(())
}

pub(crate) fn remove_unnecessary_dependencies(
    configuration: &Configuration,
) -> anyhow::Result<()> {
    let unnecessary_dependencies = get_unnecessary_dependencies(configuration)?;
    for (pack, dependency_names) in unnecessary_dependencies.iter() {
        remove_reference_to_dependency(pack, dependency_names);
    }
    Ok(())
}

pub(crate) fn check_unnecessary_dependencies(
    configuration: &Configuration,
) -> anyhow::Result<()> {
    let unnecessary_dependencies = get_unnecessary_dependencies(configuration)?;
    if unnecessary_dependencies.is_empty() {
        Ok(())
    } else {
        for (pack, dependency_names) in unnecessary_dependencies.iter() {
            for dependency_name in dependency_names {
                println!(
                    "{} depends on {} but does not use it",
                    pack.name, dependency_name
                )
            }
        }
        bail!("List unnecessary dependencies failed")
    }
}

fn get_unnecessary_dependencies(
    configuration: &Configuration,
) -> anyhow::Result<HashMap<Pack, Vec<String>>> {
    let references =
        get_all_references(configuration, &configuration.included_files)?;
    let mut edge_counts: HashMap<(String, String), i32> = HashMap::new();
    for reference in references {
        let defining_pack_name = reference.defining_pack_name;
        if let Some(defining_pack_name) = defining_pack_name {
            let edge_key =
                (reference.referencing_pack_name, defining_pack_name);

            edge_counts
                .entry(edge_key)
                .and_modify(|f| *f += 1)
                .or_insert(1);
        }
    }

    let mut unnecessary_dependencies: HashMap<Pack, Vec<String>> =
        HashMap::new();
    for pack in &configuration.pack_set.packs {
        for dependency_name in &pack.dependencies {
            let edge_key = (pack.name.clone(), dependency_name.clone());
            let edge_count = edge_counts.get(&edge_key).unwrap_or(&0);
            if edge_count == &0 {
                unnecessary_dependencies
                    .entry(pack.clone())
                    .or_default()
                    .push(dependency_name.clone());
            }
        }
    }

    Ok(unnecessary_dependencies)
}

fn get_all_violations(
    configuration: &Configuration,
    absolute_paths: &HashSet<PathBuf>,
    checkers: &Vec<Box<dyn CheckerInterface + Send + Sync>>,
) -> anyhow::Result<HashSet<Violation>> {
    let references = get_all_references(configuration, absolute_paths)?;

    debug!("Running checkers on resolved references");

    let violations = checkers
        .into_par_iter()
        .try_fold(HashSet::new, |mut acc, c| {
            for reference in &references {
                if let Some(violation) = c.check(reference, configuration)? {
                    acc.insert(violation);
                }
            }
            Ok(acc)
        })
        .try_reduce(HashSet::new, |mut acc, v| {
            acc.extend(v);
            Ok(acc)
        });

    debug!("Finished running checkers");

    violations
}

fn get_checkers(
    configuration: &Configuration,
) -> Vec<Box<dyn CheckerInterface + Send + Sync>> {
    vec![
        Box::new(dependency::Checker {}),
        Box::new(privacy::Checker {}),
        Box::new(visibility::Checker {}),
        Box::new(architecture::Checker {
            layers: configuration.layers.clone(),
        }),
        Box::new(folder_visibility::Checker {}),
    ]
}

fn remove_reference_to_dependency(pack: &Pack, dependency_names: &[String]) {
    let without_dependency = pack
        .dependencies
        .iter()
        .filter(|dependency| !dependency_names.contains(dependency));
    let updated_pack = Pack {
        dependencies: without_dependency.cloned().collect(),
        ..pack.clone()
    };
    write_pack_to_disk(&updated_pack);
}
