use crate::prelude::*;

const CFG_PATH: &str = "gaze.conf";
const DEFAULT_CFG: &str = r#"
{
    "font_height": 20,
    "left_bar": 100,
    "linenum_pad": 10,
    "linenum_color": [102, 102, 102, 255],
    "text_color": [255, 255, 255, 255],
    "bg_color": [3, 3, 4, 255],
    "scrollbar_color": [50, 50, 50, 120],
    "scrollhandle_color": [170, 170, 170, 100],
    "scrollbar_width": 20,
    "log_segment_load": false,
    "log_frame_timing": false
}
"#;

#[derive(Serialize, Deserialize, Clone)]
pub struct Cfg {
    /// In pixels.
    pub font_height: f32,
    pub left_bar: f32,
    pub linenum_pad: f32,
    pub linenum_color: [u8; 4],
    pub text_color: [u8; 4],
    pub bg_color: [u8; 4],
    pub scrollbar_color: [u8; 4],
    pub scrollhandle_color: [u8; 4],
    pub scrollbar_width: f32,
    pub log_segment_load: bool,
    pub log_frame_timing: bool,
}
impl Default for Cfg {
    fn default() -> Self {
        serde_json::from_str(DEFAULT_CFG).expect("internal error: invalid default config")
    }
}
impl Cfg {
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
        let file = File::open(path)?;
        let cfg = serde_json::from_reader(io::BufReader::new(file))?;
        Ok(cfg)
    }

    pub fn load_or_new() -> Self {
        if let Some(path) = Self::load_path() {
            match Self::load(&path) {
                Ok(cfg) => {
                    eprintln!("loaded config from \"{}\"", path.display());
                    return cfg;
                }
                Err(err) => {
                    eprintln!(
                        "WARNING: could not load config from \"{}\": {:#}",
                        path.display(),
                        err
                    );
                }
            }
        }
        let cfg = Self::default();
        if let Some(save_path) = Self::near_exe() {
            if save_path.exists() {
                eprintln!(
                    "not saving default config: file already exists at \"{}\"",
                    save_path.display()
                );
            } else {
                match cfg.save_to(&save_path) {
                    Ok(()) => eprintln!("saved default config to \"{}\"", save_path.display()),
                    Err(err) => eprintln!(
                        "WARNING: could not save config to \"{}\": {:#}",
                        save_path.display(),
                        err
                    ),
                }
            }
        }
        cfg
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
    serde_json::from_str::<Cfg>(DEFAULT_CFG).unwrap();
}
