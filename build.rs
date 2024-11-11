use std::{
    env,
    error::Error,
    fs::{create_dir_all, read_dir},
    path::Path,
    process::Command,
};

fn main() -> Result<(), Box<dyn Error>> {
    if env::var("GEN_GETTEXT").is_err() {
        return Ok(());
    }

    create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/mo/"))?;

    let po_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/po/");

    for i in read_dir(po_dir)?.flatten() {
        let p = i.path();
        let filename_without_exit = p
            .with_extension("")
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap();
        if p.extension().is_some_and(|x| x == "po") {
            let mo_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("mo")
                .join(&filename_without_exit)
                .join("LC_MESSAGE");

            create_dir_all(&mo_dir)?;

            Command::new("msgfmt")
                .arg(p)
                .arg("-o")
                .arg(mo_dir.join(format!("{}.mo", filename_without_exit)))
                .output()?;
        }
    }

    Ok(())
}
