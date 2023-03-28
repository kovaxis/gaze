use crate::prelude::*;

const CFG_PATH: &str = "gaze.conf";
const DEFAULT_CFG: &str = r#"
[ui]
# Grab button and hold
# 1 is right click
grab_button = { button = 1, hold = true }
# Invert the vertical scrolling direction when scrolling with the mouse/trackpad wheel.
invert_wheel_y = false
# Invert the horizontal scrolling direction when scrolling with the mouse/trackpad wheel.
invert_wheel_x = false
# Scrollbar button and hold
# 0 is left click
scrollbar_button = { button = 0, hold = true }
# Modifies the behaviour when clicking on the scrollbar but outside the scrollbar handle.
drag_scrollbar = false
# Slide button and hold
# 2 is middle click
slide_button = { button = 2, hold = false }
# Pixels of dead area when sliding with the middle click
slide_dead_area = 20
# Base sliding speed, in lines per second
slide_speed = 20
# The base sliding speed is this amount of screensizes away from the center
slide_base_dist = 0.10
# Every this amount of screensizes the sliding speed is doubled
slide_double_dist = 0.055

[visual]
# Height in pixels of a line of text.
font_height = 20
# Width of the line number bar.
left_bar = 100
# Padding between the line numbers and the text window.
linenum_pad = 10
# Color of the line number text.
linenum_color = [102, 102, 102, 255]
# Color of the main editor text.
text_color = [255, 255, 255, 255]
# Background color.
bg_color = [3, 3, 4, 255]
# Color of the scrollbar background.
scrollbar_color = [10, 10, 10, 220]
# Color of the corner square between the vertical and horizontal scrollbars
scrollcorner_color = [15, 15, 15, 220]
# Color of the scrollbar handle.
scrollhandle_color = [150, 150, 150, 255]
# Width of the scrollbar.
scrollbar_width = 18
# Minimum height of the scrollbar handle.
scrollhandle_min_size = 10
# Icon shown while scroll-sliding.
slide_icon = { radius = 24, detail = 20, bg = [255, 255, 255, 255], fg = [0, 0, 0, 255], arrow_shift = 14, arrow_size = 7 }

[log]
# Log the time that each rendering stage takes
frame_timing = false
# Log whenever a segment of the file is loaded to memory
segment_load = false
# Log the time taken by each load stage
# Only relevant if `segment_load` is true
segment_timing = false
# Log verbosely all of the loaded segments after loading a segment
# Only relevant if `segment_load` is true
segment_details = false

[file]
# Control the amount of memory used to cache file offset <-> text position mappings
# More memory speeds up rendering as characters can be looked up faster
linemap_mem = { fract = 0.02, min_mb = 1, max_mb = 128 }
# How many anchors to migrate in one go
# Using a large value may cause stutters
migrate_batch_size = 100000
# How much file to read in one go
read_size = 1000000
# How far away from the screen to preload file data.
load_radius = 1000000
"#;

#[derive(Serialize, Deserialize, Clone)]
pub struct SlideIcon {
    pub radius: f32,
    pub detail: usize,
    pub bg: [u8; 4],
    pub fg: [u8; 4],
    pub arrow_size: f32,
    pub arrow_shift: f32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Visual {
    /// In pixels.
    pub font_height: f32,
    pub left_bar: f32,
    pub linenum_pad: f32,
    pub linenum_color: [u8; 4],
    pub text_color: [u8; 4],
    pub bg_color: [u8; 4],
    pub selection_color: [u8; 4],
    pub selection_offset: f32,
    pub scrollbar_color: [u8; 4],
    pub scrollhandle_color: [u8; 4],
    pub scrollcorner_color: [u8; 4],
    pub scrollbar_width: f32,
    pub scrollhandle_min_size: f32,
    pub slide_icon: SlideIcon,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Log {
    pub frame_timing: bool,
    pub segment_load: bool,
    pub segment_details: bool,
    pub segment_timing: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LineMapMem {
    pub fract: f64,
    pub min_mb: f64,
    pub max_mb: f64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FileLoading {
    pub linemap_mem: LineMapMem,
    pub migrate_batch_size: usize,
    pub read_size: usize,
    pub load_radius: usize,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct DragButton {
    pub button: u16,
    pub hold: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Ui {
    pub invert_wheel_x: bool,
    pub invert_wheel_y: bool,
    pub grab_button: DragButton,
    pub scrollbar_button: DragButton,
    pub drag_scrollbar: bool,
    pub slide_button: DragButton,
    pub slide_dead_area: f64,
    pub slide_speed: f64,
    pub slide_base_dist: f64,
    pub slide_double_dist: f64,
    pub select_button: u16,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Cfg {
    #[serde(rename = "visual")]
    pub g: Visual,
    #[serde(rename = "file")]
    pub f: FileLoading,
    pub ui: Ui,
    pub log: Log,
}
impl Default for Cfg {
    fn default() -> Self {
        toml::from_str(DEFAULT_CFG).expect("internal error: invalid default config")
    }
}
impl Cfg {
    pub fn file_view_bounds(&self, (w, h): (u32, u32)) -> (Vec2, Vec2) {
        (
            vec2(self.g.left_bar, 0.),
            vec2(w as f32 - self.g.left_bar, h as f32),
        )
    }

    pub fn near_exe() -> Option<PathBuf> {
        let mut near_exe = std::env::current_exe().ok()?;
        near_exe.pop();
        near_exe.push(CFG_PATH);
        Some(near_exe)
    }

    pub fn load_path() -> Option<PathBuf> {
        let mut cur = PathBuf::from(".");
        cur.push(CFG_PATH);
        if cur.exists() {
            return Some(cur);
        }

        let near_exe = Self::near_exe()?;
        if near_exe.exists() {
            return Some(near_exe);
        }

        None
    }

    pub fn load(path: &Path) -> Result<Self> {
        let file = fs::read_to_string(path)?;
        let cfg = toml::from_str(&file)?;
        Ok(cfg)
    }

    pub fn load_or_new() -> Self {
        if let Some(path) = Self::load_path() {
            match Self::load(&path) {
                Ok(cfg) => {
                    println!("loaded config from \"{}\"", path.display());
                    return cfg;
                }
                Err(err) => {
                    println!(
                        "WARNING: could not load config from \"{}\": {:#}",
                        path.display(),
                        err
                    );
                }
            }
        }
        let default = Self::default();
        if let Some(save_path) = Self::near_exe() {
            if save_path.exists() {
                println!(
                    "not saving default config: file already exists at \"{}\"",
                    save_path.display()
                );
            } else {
                match default.save_to(&save_path) {
                    Ok(()) => println!("saved default config to \"{}\"", save_path.display()),
                    Err(err) => println!(
                        "WARNING: could not save config to \"{}\": {:#}",
                        save_path.display(),
                        err
                    ),
                }
            }
        }
        default
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let mut file = File::create(path)?;
        file.write_all(DEFAULT_CFG.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
#[test]
fn check_default_cfg() {
    Cfg::default();
}
