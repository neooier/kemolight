use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    brightness::{DisplayId, DisplayInfo},
    config::config_path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfileSlot {
    Bright,
    Dark,
}

impl ProfileSlot {
    pub const ALL: [Self; 2] = [Self::Bright, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bright => "Bright",
            Self::Dark => "Dark",
        }
    }

    pub fn shortcut(self) -> &'static str {
        match self {
            Self::Bright => "Shift+Win+B",
            Self::Dark => "Shift+Win+D",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedBrightness {
    pub name: String,
    pub brightness: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrightnessProfiles {
    pub bright: BTreeMap<String, SavedBrightness>,
    pub dark: BTreeMap<String, SavedBrightness>,
}

impl BrightnessProfiles {
    pub fn get(&self, slot: ProfileSlot) -> &BTreeMap<String, SavedBrightness> {
        match slot {
            ProfileSlot::Bright => &self.bright,
            ProfileSlot::Dark => &self.dark,
        }
    }

    pub fn capture(&mut self, slot: ProfileSlot, displays: &[DisplayInfo]) -> usize {
        let saved = match slot {
            ProfileSlot::Bright => &mut self.bright,
            ProfileSlot::Dark => &mut self.dark,
        };
        let mut count = 0;
        for display in displays {
            if display.supported && persistent_id(&display.id) {
                if let Some(brightness) = display.current {
                    saved.insert(
                        display.id.as_str().into(),
                        SavedBrightness {
                            name: display.name.clone(),
                            brightness: brightness.min(100),
                        },
                    );
                    count += 1;
                }
            }
        }
        // Entries for closed/disconnected screens stay intact.
        count
    }

    pub fn targets(&self, slot: ProfileSlot, displays: &[DisplayInfo]) -> Vec<(DisplayId, u32)> {
        displays
            .iter()
            .filter(|display| display.supported && persistent_id(&display.id))
            .filter_map(|display| {
                self.get(slot)
                    .get(display.id.as_str())
                    .map(|saved| (display.id.clone(), saved.brightness.min(100)))
            })
            .collect()
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|err| format!("Cannot read brightness profiles: {err}")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("Cannot read brightness profiles: {error}")),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let save = || -> Result<(), Box<dyn std::error::Error>> {
            let parent = path.parent().ok_or("missing profiles directory")?;
            fs::create_dir_all(parent)?;
            let text = toml::to_string_pretty(self)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            file.write_all(text.as_bytes())?;
            file.as_file().sync_all()?;
            file.persist(path)?;
            Ok(())
        };
        save().map_err(|err| format!("Cannot save brightness profiles: {err}"))
    }
}

pub fn profiles_path() -> Option<PathBuf> {
    config_path().map(|path| path.with_file_name("profiles.toml"))
}

pub fn persistent_id(id: &DisplayId) -> bool {
    !id.as_str().starts_with("windows-ddcci:temporary:")
}

#[cfg(all(test, feature = "mock-backend"))]
mod tests {
    use super::*;
    use crate::brightness::{mock::MockBrightnessBackend, BrightnessBackend};

    #[test]
    fn profiles_persist_per_display_and_preserve_disconnected_screens() {
        let backend = MockBrightnessBackend::default();
        let mut displays = backend.list_displays().unwrap();
        let mut profiles = BrightnessProfiles::default();
        profiles.capture(ProfileSlot::Bright, &displays);
        displays[0].current = Some(15);
        displays[1].current = Some(25);
        profiles.capture(ProfileSlot::Dark, &displays);
        displays.remove(0);
        displays[0].current = Some(30);
        profiles.capture(ProfileSlot::Dark, &displays);
        assert_eq!(profiles.dark["mock:internal"].brightness, 15);
        assert_eq!(profiles.dark["mock:external"].brightness, 30);
        assert_eq!(profiles.bright["mock:external"].brightness, 70);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        profiles.save(&path).unwrap();
        profiles.save(&path).unwrap(); // Replacing an existing save also works on Windows.
        assert_eq!(BrightnessProfiles::load(&path).unwrap(), profiles);
    }

    #[test]
    fn matching_uses_identity_and_skips_unknown_or_unsupported_screens() {
        let mut displays = MockBrightnessBackend::default().list_displays().unwrap();
        let mut profiles = BrightnessProfiles::default();
        profiles.capture(ProfileSlot::Dark, &displays);
        displays.reverse();
        displays[0].supported = false;
        let targets = profiles.targets(ProfileSlot::Dark, &displays);
        assert_eq!(targets, vec![(DisplayId::new("mock:internal"), 60)]);
        displays[1].id = DisplayId::new("another-screen");
        assert!(profiles.targets(ProfileSlot::Dark, &displays).is_empty());
        displays[1].id = DisplayId::new("windows-ddcci:temporary:0:0");
        assert_eq!(profiles.capture(ProfileSlot::Bright, &displays[1..]), 0);
    }
}
