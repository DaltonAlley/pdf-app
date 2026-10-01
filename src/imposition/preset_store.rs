use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use super::{
    model::{
        ArtworkFit, ArtworkFitMode, BleedOption, CropPosition, FinishedSizeMode, GangUpPreset,
        GuttersInches, ImpositionMode, LayoutMode, OrientationPreference, OutputPreference,
        PresetInput, Sides, SizeInches,
    },
    persistence::{read_json_or_default, valid_identifier, write_json_atomic, MAX_SAVED_RECORDS},
    validation::validate_preset_input,
};
use crate::error::{lock_mutex, AppError, AppResult};

const PRESET_FILE: &str = "gang-up-presets.json";

// Keep initialization and records in the same atomic write. A separate marker
// could resurrect a deleted seed after a crash between the two writes.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedPresets {
    initialized: bool,
    presets: Vec<GangUpPreset>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum StoredPresets {
    Current(SavedPresets),
    Legacy(Vec<GangUpPreset>),
}

impl Default for StoredPresets {
    fn default() -> Self {
        Self::Legacy(Vec::new())
    }
}

#[derive(Clone)]
pub(crate) struct PresetStore {
    path: Arc<PathBuf>,
    write_lock: Arc<Mutex<()>>,
}

impl PresetStore {
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        Self {
            path: Arc::new(data_dir.join(PRESET_FILE)),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn list(&self) -> AppResult<Vec<GangUpPreset>> {
        self.read_saved()
    }

    pub(crate) fn create(&self, input: PresetInput) -> AppResult<GangUpPreset> {
        validate_preset_input(&input)?;
        let _guard = lock_mutex(&self.write_lock, "preset store")?;
        let mut saved = self.read_saved_unlocked()?;
        let id = input
            .id
            .clone()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| unique_preset_id(&input.name, &saved));
        if !valid_identifier(&id) {
            return Err(AppError::bad_request(
                "preset id contains unsupported characters",
            ));
        }
        if saved.iter().any(|preset| preset.id == id) {
            return Err(AppError::bad_request("preset id already exists"));
        }
        if saved.len() >= MAX_SAVED_RECORDS {
            return Err(AppError::bad_request(format!(
                "no more than {MAX_SAVED_RECORDS} saved presets are supported"
            )));
        }
        let preset = preset_from_input(id, input);
        saved.push(preset.clone());
        self.write_saved_unlocked(&saved)?;
        Ok(preset)
    }

    pub(crate) fn update(&self, id: &str, input: PresetInput) -> AppResult<GangUpPreset> {
        validate_preset_input(&input)?;
        let _guard = lock_mutex(&self.write_lock, "preset store")?;
        let mut saved = self.read_saved_unlocked()?;
        let index = saved
            .iter()
            .position(|preset| preset.id == id)
            .ok_or_else(|| AppError::bad_request("preset not found"))?;
        let preset = preset_from_input(id.to_string(), input);
        let saved_preset = saved
            .get_mut(index)
            .ok_or_else(|| AppError::Internal("saved preset index became invalid".to_string()))?;
        *saved_preset = preset.clone();
        self.write_saved_unlocked(&saved)?;
        Ok(preset)
    }

    pub(crate) fn delete(&self, id: &str) -> AppResult<()> {
        let _guard = lock_mutex(&self.write_lock, "preset store")?;
        let mut saved = self.read_saved_unlocked()?;
        let before = saved.len();
        saved.retain(|preset| preset.id != id);
        if saved.len() == before {
            return Err(AppError::bad_request("preset not found"));
        }
        self.write_saved_unlocked(&saved)
    }

    fn read_saved(&self) -> AppResult<Vec<GangUpPreset>> {
        let _guard = lock_mutex(&self.write_lock, "preset store")?;
        self.read_saved_unlocked()
    }

    fn read_saved_unlocked(&self) -> AppResult<Vec<GangUpPreset>> {
        let stored: StoredPresets = read_json_or_default(self.path.as_path(), "saved presets")?;
        let (mut presets, initialized) = match stored {
            StoredPresets::Current(saved) => (saved.presets, saved.initialized),
            StoredPresets::Legacy(presets) => (presets, false),
        };
        let mut ids = HashSet::with_capacity(presets.len());
        if presets.len() > MAX_SAVED_RECORDS
            || presets.iter().any(|preset| {
                !valid_identifier(&preset.id)
                    || !ids.insert(preset.id.as_str())
                    || validate_preset_input(&preset_as_input(preset)).is_err()
            })
        {
            return Err(AppError::Internal(
                "saved preset metadata is invalid".to_string(),
            ));
        }
        for preset in &mut presets {
            preset.built_in = false;
        }
        if !initialized {
            // A full legacy store must never lose a user record to make room
            // for the default. Mark it initialized even when no seed fits.
            if presets.len() < MAX_SAVED_RECORDS {
                let input = seed_input();
                presets.push(preset_from_input(
                    unique_preset_id(&input.name, &presets),
                    input,
                ));
            }
            self.write_saved_unlocked(&presets)?;
        }
        Ok(presets)
    }

    fn write_saved_unlocked(&self, presets: &[GangUpPreset]) -> AppResult<()> {
        write_json_atomic(
            self.path.as_path(),
            &SavedPresets {
                initialized: true,
                presets: presets.to_vec(),
            },
            "saved presets",
        )
    }
}

fn preset_as_input(preset: &GangUpPreset) -> PresetInput {
    PresetInput {
        imposition_mode: preset.imposition_mode,
        finished_size_mode: preset.finished_size_mode,
        artwork_fit: preset.artwork_fit,
        source_bleed_override: preset.source_bleed_override,
        id: Some(preset.id.clone()),
        name: preset.name.clone(),
        finished_cut_size: preset.finished_cut_size,
        parent_sheet_size: preset.parent_sheet_size,
        bleed_handling: preset.bleed_handling,
        created_bleed_amount: preset.created_bleed_amount,
        gutter: preset.gutter,
        orientation_preference: preset.orientation_preference,
        sides: preset.sides,
        layout_preference: preset.layout_preference,
        manual: preset.manual,
        duplex: preset.duplex.clone(),
        output_preference: Some(preset.output_preference),
    }
}

fn preset_from_input(id: String, input: PresetInput) -> GangUpPreset {
    GangUpPreset {
        imposition_mode: input.imposition_mode,
        finished_size_mode: input.finished_size_mode,
        artwork_fit: input.artwork_fit,
        source_bleed_override: input.source_bleed_override,
        id,
        name: input.name,
        finished_cut_size: input.finished_cut_size,
        parent_sheet_size: input.parent_sheet_size,
        bleed_handling: input.bleed_handling,
        created_bleed_amount: input.created_bleed_amount,
        gutter: input.gutter,
        orientation_preference: input.orientation_preference,
        sides: input.sides,
        layout_preference: input.layout_preference,
        manual: input.manual,
        duplex: input.duplex,
        output_preference: input.output_preference.unwrap_or_default(),
        built_in: false,
    }
}

fn seed_input() -> PresetInput {
    PresetInput {
        imposition_mode: ImpositionMode::Repeat,
        finished_size_mode: FinishedSizeMode::Common,
        artwork_fit: Some(ArtworkFit {
            mode: ArtworkFitMode::Contain,
            position: CropPosition::default(),
        }),
        source_bleed_override: None,
        id: None,
        name: "5x7 on 12x18".to_string(),
        finished_cut_size: SizeInches {
            width: 5.0,
            height: 7.0,
        },
        parent_sheet_size: SizeInches {
            width: 12.0,
            height: 18.0,
        },
        bleed_handling: BleedOption::UseAsIs,
        created_bleed_amount: 0.125,
        gutter: GuttersInches {
            horizontal: 0.299,
            vertical: 0.299,
        },
        orientation_preference: OrientationPreference::Auto,
        sides: Sides::Single,
        layout_preference: LayoutMode::MaxPieces,
        manual: None,
        duplex: None,
        output_preference: Some(OutputPreference::CleanPdf),
    }
}

fn unique_preset_id(name: &str, saved: &[GangUpPreset]) -> String {
    let base = preset_id(name);
    let mut id = base.clone();
    let mut suffix = 2;
    while saved.iter().any(|preset| preset.id == id) {
        let ending = format!("-{suffix}");
        id = format!("{}{}", &base[..base.len().min(200 - ending.len())], ending);
        suffix += 1;
    }
    id
}

fn preset_id(name: &str) -> String {
    let id = name
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if id.is_empty() {
        "preset".to_string()
    } else {
        id
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{
        BleedOption, GuttersInches, LayoutMode, OrientationPreference, OutputPreference, Sides,
        SizeInches,
    };
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn store() -> PresetStore {
        static NEXT_DIR: AtomicU64 = AtomicU64::new(1);
        PresetStore::new(std::env::temp_dir().join(format!(
            "pdf-tools-preset-store-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn valid_input(name: &str) -> PresetInput {
        PresetInput {
            imposition_mode: ImpositionMode::Repeat,
            finished_size_mode: Default::default(),
            artwork_fit: None,
            source_bleed_override: None,
            id: None,
            name: name.to_string(),
            finished_cut_size: SizeInches {
                width: 3.5,
                height: 2.0,
            },
            parent_sheet_size: SizeInches {
                width: 12.0,
                height: 18.0,
            },
            bleed_handling: BleedOption::UseAsIs,
            created_bleed_amount: 0.125,
            gutter: GuttersInches {
                horizontal: 0.299,
                vertical: 0.299,
            },
            orientation_preference: OrientationPreference::Auto,
            sides: Sides::Single,
            layout_preference: LayoutMode::Auto,
            manual: None,
            duplex: None,
            output_preference: Some(OutputPreference::CleanPdf),
        }
    }

    #[test]
    fn store_starts_seeded_and_manages_user_saved_presets() {
        let store = store();
        let seed = store.list().unwrap().remove(0);

        let created = store.create(valid_input("Business card")).unwrap();
        assert_eq!(created.id, "business-card");
        assert!(!created.built_in);
        assert_eq!(store.list().unwrap(), vec![seed.clone(), created.clone()]);

        let mut updated_input = valid_input("Business card production");
        updated_input.parent_sheet_size.width = 13.0;
        let updated = store.update(&created.id, updated_input).unwrap();
        assert_eq!(updated.id, created.id);
        assert_eq!(updated.name, "Business card production");
        assert_eq!(updated.parent_sheet_size.width, 13.0);

        store.delete(&created.id).unwrap();
        assert_eq!(store.list().unwrap(), vec![seed]);
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn stretch_preset_survives_disk_reload_and_can_return_to_fit() {
        use super::super::model::{ArtworkFit, ArtworkFitMode, CropPosition};
        let store = store();
        let seed = store.list().unwrap().remove(0);
        let mut input = valid_input("Stretch preset");
        input.artwork_fit = Some(ArtworkFit {
            mode: ArtworkFitMode::Stretch,
            position: CropPosition::default(),
        });
        let created = store.create(input.clone()).unwrap();
        let reloaded = PresetStore::new(store.path.parent().unwrap().to_path_buf());
        assert_eq!(
            reloaded.list().unwrap(),
            vec![seed.clone(), created.clone()]
        );
        input.artwork_fit.as_mut().unwrap().mode = ArtworkFitMode::Contain;
        let updated = reloaded.update(&created.id, input).unwrap();
        assert_eq!(store.list().unwrap(), vec![seed, updated]);
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn seed_is_explicit_editable_and_never_returns_after_delete() {
        let store = store();
        let seed = store.list().unwrap().remove(0);
        assert_eq!(seed, preset_from_input("5x7-on-12x18".into(), seed_input()));
        assert_eq!(seed.finished_size_mode, FinishedSizeMode::Common);
        assert_eq!(seed.artwork_fit.unwrap().mode, ArtworkFitMode::Contain);
        assert_eq!(seed.artwork_fit.unwrap().position, CropPosition::default());
        assert!(!seed.built_in);
        let mut edited = preset_as_input(&seed);
        edited.name = "My edited default".into();
        edited.imposition_mode = ImpositionMode::Unique;
        edited.finished_cut_size.width = 6.0;
        let updated = store.update(&seed.id, edited).unwrap();
        let reloaded = PresetStore::new(store.path.parent().unwrap().to_path_buf());
        assert_eq!(reloaded.list().unwrap(), vec![updated]);
        reloaded.delete(&seed.id).unwrap();
        let reloaded = PresetStore::new(store.path.parent().unwrap().to_path_buf());
        assert!(reloaded.list().unwrap().is_empty());
        assert!(store.list().unwrap().is_empty());
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.path.as_path()).unwrap()).unwrap();
        assert_eq!(disk["initialized"], true);
        assert_eq!(disk["presets"], serde_json::json!([]));
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn legacy_records_keep_fields_and_default_missing_mode_during_one_time_migration() {
        let store = store();
        let old = preset_from_input("5x7-on-12x18".into(), valid_input("My old preset"));
        let mut legacy = serde_json::to_value(&old).unwrap();
        legacy.as_object_mut().unwrap().remove("impositionMode");
        write_json_atomic(store.path.as_path(), &[legacy.clone()], "legacy presets").unwrap();
        let input: PresetInput = serde_json::from_value(legacy).unwrap();
        assert_eq!(input.imposition_mode, ImpositionMode::Repeat);
        let migrated = store.list().unwrap();
        assert_eq!(migrated.len(), 2);
        assert_eq!(migrated[0], old);
        assert_eq!(migrated[1].id, "5x7-on-12x18-2");
        let reloaded = PresetStore::new(store.path.parent().unwrap().to_path_buf());
        assert_eq!(reloaded.list().unwrap(), migrated);
        reloaded.delete(&migrated[1].id).unwrap();
        assert_eq!(store.list().unwrap(), vec![old]);
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn automatic_ids_get_suffixes_but_explicit_duplicates_are_rejected() {
        let store = store();
        assert_eq!(
            store.create(valid_input("Same name")).unwrap().id,
            "same-name"
        );
        let mut input = valid_input("Another name");
        input.id = Some("same-name-2".into());
        store.create(input.clone()).unwrap();
        assert!(store.create(input).is_err());
        assert_eq!(
            store.create(valid_input("Same name")).unwrap().id,
            "same-name-3"
        );
        let long_name = "a".repeat(200);
        store.create(valid_input(&long_name)).unwrap();
        let suffixed = store.create(valid_input(&long_name)).unwrap();
        assert_eq!(suffixed.id.len(), 200);
        assert!(suffixed.id.ends_with("-2"));
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn full_legacy_store_preserves_all_records_without_later_reseeding() {
        let store = store();
        let records = (0..MAX_SAVED_RECORDS)
            .map(|index| preset_from_input(format!("preset-{index}"), valid_input("Saved preset")))
            .collect::<Vec<_>>();
        write_json_atomic(store.path.as_path(), &records, "legacy presets").unwrap();
        assert_eq!(store.list().unwrap(), records);
        store.delete("preset-0").unwrap();
        let reloaded = PresetStore::new(store.path.parent().unwrap().to_path_buf());
        assert_eq!(reloaded.list().unwrap(), records[1..]);
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn both_modes_survive_create_serialization_and_reload() {
        let store = store();
        for mode in [ImpositionMode::Repeat, ImpositionMode::Unique] {
            let mut input = valid_input("Mode roundtrip");
            input.imposition_mode = mode;
            let input = serde_json::from_value(serde_json::to_value(input).unwrap()).unwrap();
            let created = store.create(input).unwrap();
            let encoded = serde_json::to_value(&created).unwrap();
            assert_eq!(encoded["outputPreference"], "cleanPdf");
            assert_eq!(
                encoded["impositionMode"],
                serde_json::to_value(mode).unwrap()
            );
            let reloaded = PresetStore::new(store.path.parent().unwrap().to_path_buf());
            assert!(reloaded.list().unwrap().contains(&created));
        }
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn concurrent_first_creates_seed_once_and_keep_unique_ids() {
        let store = store();
        let workers = (0..8)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || store.create(valid_input("Concurrent preset")).unwrap())
            })
            .collect::<Vec<_>>();
        let created = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        let saved = store.list().unwrap();
        assert_eq!(saved.len(), 9);
        assert_eq!(
            saved
                .iter()
                .filter(|preset| preset.name == "5x7 on 12x18")
                .count(),
            1
        );
        for preset in created {
            assert!(saved.contains(&preset));
        }
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_legacy_metadata_is_not_overwritten_or_marked_initialized() {
        let store = store();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        let malformed = b"[{\"id\":\"unfinished\"";
        std::fs::write(store.path.as_path(), malformed).unwrap();
        assert!(store.list().is_err());
        assert_eq!(std::fs::read(store.path.as_path()).unwrap(), malformed);
        // Repairing the old JSON still gets its one-time seed.
        std::fs::write(store.path.as_path(), b"[]").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(
            std::fs::read_dir(store.path.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn store_rejects_invalid_inputs_even_without_route_preflight() {
        let store = store();
        let mut input = valid_input("Invalid preset");
        input.id = Some("invalid-preset".to_string());
        input.gutter.horizontal = f64::NAN;

        assert!(store.create(input).is_err());
        let _ = std::fs::remove_dir_all(store.path.parent().unwrap());
    }

    #[test]
    fn store_rejects_persisted_records_that_violate_invariants() {
        let store = store();
        let mut preset = preset_from_input(
            "invalid-persisted-preset".to_string(),
            valid_input("Invalid persisted preset"),
        );
        preset.finished_cut_size.width = 0.0;
        write_json_atomic(store.path.as_path(), &[preset], "test presets").unwrap();

        assert!(store.list().is_err());
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn store_rejects_duplicate_persisted_ids() {
        let store = store();
        let preset = preset_from_input(
            "duplicate-preset".to_string(),
            valid_input("Duplicate preset"),
        );
        write_json_atomic(
            store.path.as_path(),
            &[preset.clone(), preset],
            "test presets",
        )
        .unwrap();
        assert!(store.list().is_err());
        std::fs::remove_dir_all(store.path.parent().unwrap()).unwrap();
    }
}
