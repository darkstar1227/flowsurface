use super::{IndicatorId, catalog, language};
use std::path::PathBuf;

#[derive(Clone, Copy)]
struct BundledSource {
    file_name: &'static str,
    source: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/indicator_bundled_sources.rs"));

pub(super) fn validate_bundled_catalog(expected_ids: &[IndicatorId]) -> Result<(), String> {
    let catalog = catalog();
    let missing = expected_ids
        .iter()
        .filter(|id| catalog.get(id).is_none())
        .map(|id| id.id().to_owned())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "missing bundled indicator source(s): {}",
            missing.join(", ")
        ))
    }
}

pub(super) fn load_bundled_sources() -> language::IndicatorLoadReport {
    language::load_compiled_report(
        BUNDLED_SOURCES.iter().map(|source| {
            let path = PathBuf::from("indicators/sources").join(source.file_name);
            let compiled = indicator_engine::compile(source.source, &path);
            (path, compiled)
        }),
        language::PackageOrigin::Bundled,
    )
}

/// The embedded source text of a bundled indicator, for read-only viewing.
pub(super) fn source_text(id: &IndicatorId) -> Option<&'static str> {
    let file_name = id
        .id()
        .rsplit_once('.')
        .map(|(_, name)| format!("{name}.ind"))?;
    BUNDLED_SOURCES
        .iter()
        .find(|source| source.file_name == file_name)
        .map(|source| source.source)
}
