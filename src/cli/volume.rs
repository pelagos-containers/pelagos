//! `remora volume` — manage named volumes.

use pelagos::container::Volume;

pub fn cmd_volume_create(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    Volume::create(name).map_err(|e| format!("create volume '{}': {}", name, e))?;
    println!("Created volume '{}'", name);
    Ok(())
}

pub fn cmd_volume_ls(json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let volumes_dir = pelagos::paths::volumes_dir();
    let entries = match std::fs::read_dir(&volumes_dir) {
        Ok(e) => e,
        Err(_) => {
            if json {
                println!("[]");
            } else {
                println!("No volumes found.");
            }
            return Ok(());
        }
    };

    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    if json {
        #[derive(serde::Serialize)]
        struct VolumeInfo {
            name: String,
            path: String,
        }
        let infos: Vec<VolumeInfo> = names
            .iter()
            .map(|n| VolumeInfo {
                name: n.clone(),
                path: volumes_dir.join(n).to_string_lossy().into_owned(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&infos)?);
        return Ok(());
    }

    if names.is_empty() {
        println!("No volumes. Use: remora volume create <name>");
        return Ok(());
    }

    println!("NAME");
    for name in &names {
        println!("{}", name);
    }
    Ok(())
}

pub fn cmd_volume_rm(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    Volume::delete(name).map_err(|e| format!("remove volume '{}': {}", name, e))?;
    println!("Removed volume '{}'", name);
    Ok(())
}
