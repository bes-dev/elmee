//! What the endpoints last said they serve, kept on disk so that neither a
//! picker nor startup needs the network.

use super::*;

/// What the endpoints said they serve the last time each answered, and the
/// context window of every model that stated one. One file, because it is one
/// answer: a picker opens on its names and compaction reads its numbers.
type Catalogue = BTreeMap<String, Option<usize>>;

fn catalogue_cache() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".elmee/model-catalogue.json"))
}

fn read_catalogue() -> Catalogue {
    catalogue_cache()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

/// What endpoints have said about their own models' windows, read once.
pub(super) fn learned_windows() -> &'static BTreeMap<String, usize> {
    static TABLE: std::sync::OnceLock<BTreeMap<String, usize>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        read_catalogue().into_iter().filter_map(|(spec, window)| Some((spec, window?))).collect()
    })
}

/// When the endpoints were last asked what they serve, in this process.
static ASKED: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// How long an answer about what the endpoints serve is taken as current.
const CATALOGUE_AGE: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether the endpoints were asked recently enough not to be asked again.
pub fn catalogue_is_fresh() -> bool {
    ASKED.lock().unwrap().is_some_and(|asked| asked.elapsed() < CATALOGUE_AGE)
}

/// Every model this machine can name without asking anyone: the ones its
/// files declare, then what each endpoint said the last time it answered.
///
/// A picker opens on this at once. Asking the endpoints is a network round
/// trip each, and one that is switched off takes its whole timeout to say
/// nothing — which is not something to sit through every time a list is
/// opened to look at.
pub fn known_models(cwd: &Path) -> Result<Vec<String>> {
    let registry = load(cwd)?;
    let mut known: Vec<String> = registry
        .entries
        .iter()
        .flat_map(|entry| entry.declared.iter().map(move |model| crate::spec(&entry.name, model)))
        .collect();
    let served =
        |spec: &str| registry.entries.iter().any(|entry| spec.starts_with(&format!("{}/", entry.name)));
    for spec in read_catalogue().into_keys() {
        if served(&spec) && !known.contains(&spec) {
            known.push(spec);
        }
    }
    Ok(known)
}

/// Keep what the endpoints just said, so the next start knows it without
/// asking. An endpoint that did not answer keeps what it said last: being
/// unreachable today is not having stopped serving anything.
pub(crate) fn remember_models(answered: &[crate::Model], silent: &[&str]) {
    *ASKED.lock().unwrap() = Some(std::time::Instant::now());
    let Some(path) = catalogue_cache() else { return };
    let before = read_catalogue();
    let mut seen: Catalogue = before
        .iter()
        .filter(|(spec, _)| silent.iter().any(|provider| spec.starts_with(&format!("{provider}/"))))
        .map(|(spec, window)| (spec.clone(), *window))
        .collect();
    seen.extend(answered.iter().map(|model| (model.id.clone(), model.context_window)));
    if seen != before {
        // A cache: losing it costs one slower list, so it is not synced.
        let _ = elmee_plugin_atomic::write(
            &path,
            &serde_json::to_vec(&seen).unwrap_or_default(),
            elmee_plugin_atomic::Options { durable: false, ..Default::default() },
        );
    }
}
