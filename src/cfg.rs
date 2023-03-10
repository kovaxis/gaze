use crate::prelude::*;

const CFG_PATH: &str = "gaze.conf";
const DEFAULT_CFG: &str = r#"
{
    "font_height": 20,
    "left_bar": 100,
    "linenum_pad": 10,
    "linenum_color": [0.4, 0.4, 0.4, 1],
    "text_color": [1, 1, 1, 1],
    "bg_color": [0.01, 0.01, 0.012, 1]
}
"#;

#[derive(Serialize, Deserialize)]
pub struct Cfg {
    /// In pixels.
    pub font_height: f32,
    pub left_bar: f32,
    pub linenum_pad: f32,
    pub linenum_color: [f32; 4],
    pub text_color: [f32; 4],
    pub bg_color: [f32; 4],
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
                Ok(cfg) => return cfg,
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
            match cfg.save_to(&save_path) {
                Ok(()) => eprintln!("saved default config to \"{}\"", save_path.display()),
                Err(err) => eprintln!(
                    "WARNING: could not save config to \"{}\": {:#}",
                    save_path.display(),
                    err
                ),
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
