use std::cmp::Ordering;

use icu::{
    collator::{Collator, CollatorBorrowed, CollatorPreferences, options::CollatorOptions},
    locale::Locale,
};

use crate::{
    metadata::{effective_date, enrich_entries_for_time_sort},
    state::{ImageEntry, SortMode},
};

pub fn sort_entries(entries: &mut [ImageEntry], mode: SortMode, locale: Option<&str>) {
    match mode {
        SortMode::Discovered => {
            entries.sort_by_key(|entry| entry.discovered_order);
        }
        SortMode::Newest | SortMode::Oldest => {
            enrich_entries_for_time_sort(entries);
            entries.sort_by(|a, b| compare_time(a, b, mode));
        }
        SortMode::NameAsc | SortMode::NameDesc => {
            let collator = build_collator(locale);
            entries.sort_by(|a, b| compare_name(a, b, mode, collator.as_ref()));
        }
    }
}

pub fn next_time_sort(mode: SortMode) -> SortMode {
    match mode {
        SortMode::Newest => SortMode::Oldest,
        _ => SortMode::Newest,
    }
}

pub fn next_name_sort(mode: SortMode) -> SortMode {
    match mode {
        SortMode::NameAsc => SortMode::NameDesc,
        _ => SortMode::NameAsc,
    }
}

fn compare_time(a: &ImageEntry, b: &ImageEntry, mode: SortMode) -> Ordering {
    let ordering = match (effective_date(a), effective_date(b)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.discovered_order.cmp(&b.discovered_order),
    };

    match mode {
        SortMode::Newest => ordering
            .reverse()
            .then(a.discovered_order.cmp(&b.discovered_order)),
        SortMode::Oldest => ordering.then(a.discovered_order.cmp(&b.discovered_order)),
        _ => Ordering::Equal,
    }
}

fn compare_name(
    a: &ImageEntry,
    b: &ImageEntry,
    mode: SortMode,
    collator: Option<&CollatorBorrowed<'static>>,
) -> Ordering {
    let ordering = match (a.file_name.to_str(), b.file_name.to_str(), collator) {
        (Some(left), Some(right), Some(collator)) => collator.compare(left, right),
        (Some(left), Some(right), None) => left.cmp(right),
        _ => format!("{:?}", a.file_name).cmp(&format!("{:?}", b.file_name)),
    }
    .then(a.discovered_order.cmp(&b.discovered_order));

    match mode {
        SortMode::NameDesc => ordering.reverse(),
        _ => ordering,
    }
}

fn build_collator(locale: Option<&str>) -> Option<CollatorBorrowed<'static>> {
    let options = CollatorOptions::default();
    let prefs = locale
        .and_then(|value| value.parse::<Locale>().ok())
        .map(CollatorPreferences::from)
        .unwrap_or_default();

    Collator::try_new(prefs, options).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ImageKind;
    use std::{
        ffi::OsString,
        path::PathBuf,
        time::{Duration, SystemTime},
    };

    fn entry(name: &str, order: usize, modified_secs: Option<u64>) -> ImageEntry {
        ImageEntry {
            path: PathBuf::from(name),
            file_name: OsString::from(name),
            display_name: name.to_owned(),
            extension: Some("jpg".to_owned()),
            file_len: 0,
            created: None,
            modified: modified_secs.map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
            discovered_order: order,
            dimensions: None,
            image_type: Some(ImageKind::Jpeg),
            exif_date: None,
            exif_orientation: None,
        }
    }

    #[test]
    fn newest_uses_stable_fallback() {
        let mut entries = vec![entry("a.jpg", 0, Some(1)), entry("b.jpg", 1, Some(5))];

        sort_entries(&mut entries, SortMode::Newest, None);

        assert_eq!(entries[0].display_name, "b.jpg");
    }

    #[test]
    fn name_desc_is_stable() {
        let mut entries = vec![entry("b.jpg", 0, None), entry("a.jpg", 1, None)];

        sort_entries(&mut entries, SortMode::NameDesc, Some("en"));

        assert_eq!(entries[0].display_name, "b.jpg");
    }

    #[test]
    fn unicode_names_do_not_panic() {
        let mut entries = vec![entry("å.jpg", 0, None), entry("a.jpg", 1, None)];

        sort_entries(&mut entries, SortMode::NameAsc, Some("sv"));

        assert_eq!(entries.len(), 2);
    }
}
