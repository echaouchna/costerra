//! Compute Engine machine types and their SKU naming.

use crate::pricing::query::{Pricer, Query};
use std::collections::HashMap;
use std::sync::LazyLock;

pub const COMPUTE: &str = "Compute Engine";

#[derive(Debug, Clone, PartialEq)]
pub struct MachineType {
    pub name: String,
    pub family: String,
    pub vcpus: f64,
    pub memory_gb: f64,
    pub shared_core: bool,
    pub custom: bool,
    pub extended_memory_gb: f64,
    /// Bundled accelerators: (type, count).
    pub accelerators: Vec<(String, f64)>,
    /// Bundled local SSD in GiB (for *-lssd machine types).
    pub local_ssd_gb: f64,
}

/// name -> (vCPUs, memory GiB, shared core, accelerators)
type MachineTable = HashMap<&'static str, (f64, f64, bool, &'static str)>;

static TABLE: LazyLock<MachineTable> = LazyLock::new(|| {
    include_str!("../../data/machine_types.tsv")
        .lines()
        .filter_map(|l| {
            let mut p = l.split('\t');
            let name = p.next()?;
            let cpu: f64 = p.next()?.parse().ok()?;
            let mem: f64 = p.next()?.parse().ok()?;
            let shared = p.next()? == "1";
            let acc = p.next().unwrap_or("");
            Some((name, (cpu, mem / 1024.0, shared, acc)))
        })
        .collect()
});

/// Known local SSD bundled with `-lssd` machine types (number of 375 GiB disks
/// per vCPU count), approximated from the documented shapes.
fn bundled_lssd(name: &str, vcpus: f64) -> f64 {
    if !name.contains("lssd") {
        return 0.0;
    }
    let disks = if name.starts_with("c3d-") {
        (vcpus / 8.0).ceil().max(1.0)
    } else if name.starts_with("c3-") || name.starts_with("c4-") || name.starts_with("c4d-") || name.starts_with("c4a-") {
        (vcpus / 4.0).floor().max(1.0)
    } else if name.starts_with("z3-") {
        (vcpus / 5.5).floor().max(1.0) * 8.0 / 8.0
    } else {
        (vcpus / 8.0).ceil().max(1.0)
    };
    disks * 375.0
}

pub fn family_of(name: &str) -> String {
    let n = name.rsplit('/').next().unwrap_or(name);
    if n.starts_with("custom-") {
        return "n1".into();
    }
    n.split('-').next().unwrap_or("").to_lowercase()
}

/// Parse a machine type name (predefined or custom).
pub fn parse(name: &str) -> Option<MachineType> {
    let name = name.rsplit('/').next().unwrap_or(name);
    let family = family_of(name);
    if let Some((cpu, mem, shared, acc)) = TABLE.get(name) {
        let accelerators = acc
            .split(',')
            .filter(|s| !s.is_empty())
            .filter_map(|a| {
                let (t, c) = a.split_once(':')?;
                Some((t.to_string(), c.parse().ok()?))
            })
            .collect();
        return Some(MachineType {
            name: name.to_string(),
            family,
            vcpus: *cpu,
            memory_gb: *mem,
            shared_core: *shared,
            custom: false,
            extended_memory_gb: 0.0,
            accelerators,
            local_ssd_gb: bundled_lssd(name, *cpu),
        });
    }
    // custom-{cpus}-{mem_mb}[-ext], {family}-custom-{cpus}-{mem_mb}[-ext]
    let parts: Vec<&str> = name.split('-').collect();
    let ci = parts.iter().position(|p| *p == "custom")?;
    let cpus: f64 = parts.get(ci + 1)?.parse().ok()?;
    let mem_mb: f64 = parts.get(ci + 2)?.parse().ok()?;
    let ext = parts.get(ci + 3) == Some(&"ext");
    let mem_gb = mem_mb / 1024.0;
    let max_std = match family.as_str() {
        "n1" => 6.5,
        "e2" => 8.0,
        "n4" => 8.0,
        _ => 8.0,
    } * cpus;
    let extended = if ext { (mem_gb - max_std).max(0.0) } else { 0.0 };
    Some(MachineType {
        name: name.to_string(),
        family,
        vcpus: cpus,
        memory_gb: mem_gb - extended,
        shared_core: false,
        custom: true,
        extended_memory_gb: extended,
        accelerators: vec![],
        local_ssd_gb: 0.0,
    })
}

/// SKU description stems for a machine family: (core, ram, commitment name).
pub fn family_skus(family: &str, machine: &str) -> Option<(&'static str, &'static str, Option<&'static str>)> {
    Some(match family {
        "n1" => ("N1 Predefined Instance Core", "N1 Predefined Instance Ram", Some("")),
        "n2" => ("N2 Instance Core", "N2 Instance Ram", Some("N2 ")),
        "n2d" => ("N2D AMD Instance Core", "N2D AMD Instance Ram", Some("N2D AMD ")),
        "e2" => ("E2 Instance Core", "E2 Instance Ram", Some("E2 ")),
        "t2d" => ("T2D AMD Instance Core", "T2D AMD Instance Ram", Some("T2D AMD ")),
        "t2a" => ("T2A Arm Instance Core", "T2A Arm Instance Ram", None),
        "c2" => ("Compute optimized Core", "Compute optimized Ram", None),
        "c2d" => ("C2D AMD Instance Core", "C2D AMD Instance Ram", Some("C2D AMD ")),
        "c3" => ("C3 Instance Core", "C3 Instance Ram", Some("C3 ")),
        "c3d" => ("C3D Instance Core", "C3D Instance Ram", Some("C3D ")),
        "c4" => ("C4 Instance Core", "C4 Instance Ram", Some("C4 ")),
        "c4a" => ("C4A Arm Instance Core", "C4A Arm Instance Ram", Some("C4A Arm ")),
        "c4d" => ("C4D Instance Core", "C4D Instance Ram", Some("C4D ")),
        "c4n" => ("C4N Instance Core", "C4N Instance Ram", Some("C4N ")),
        "n4" => ("N4 Instance Core", "N4 Instance Ram", Some("N4 ")),
        "n4a" => ("N4A Instance Core", "N4A Instance Ram", Some("N4A ")),
        "n4d" => ("N4D Instance Core", "N4D Instance Ram", Some("N4D ")),
        "h3" => ("H3 Instance Core", "H3 Instance Ram", Some("H3 ")),
        "h4d" => ("H4D Instance Core", "H4D Instance Ram", Some("H4D ")),
        "z3" => ("Z3 Instance Core", "Z3 Instance Ram", Some("Z3 ")),
        "a2" => ("A2 Instance Core", "A2 Instance Ram", Some("A2 ")),
        "a3" => {
            if machine.contains("ultragpu") {
                ("A3Ultra Instance Core", "A3Ultra Instance Ram", Some("A3Ultra "))
            } else if machine.contains("megagpu") {
                ("A3Plus Instance Core", "A3Plus Instance Ram", Some("A3Plus "))
            } else {
                ("A3 Instance Core", "A3 Instance Ram", Some("A3 "))
            }
        }
        "g2" => ("G2 Instance Core", "G2 Instance Ram", Some("G2 ")),
        "g4" => ("G4 Instance Core", "G4 Instance Ram", Some("G4 ")),
        "m1" | "m2" => ("Memory-optimized Instance Core", "Memory-optimized Instance Ram", Some("Memory-optimized ")),
        "m3" => ("M3 Memory-optimized Instance Core", "M3 Memory-optimized Instance Ram", Some("M3 Memory-optimized ")),
        "m4" => {
            if machine.contains("ultramem") {
                ("M4Ultramem224 Instance Core", "M4Ultramem224 Instance Ram", Some("M4Ultramem224 "))
            } else {
                ("M4 Instance Core", "M4 Instance Ram", Some("M4 "))
            }
        }
        "z4d" => {
            if machine.contains("highlssd") {
                ("Z4D-HIGHMEM-HIGHLSSD Instance Core", "Z4D-HIGHMEM-HIGHLSSD Instance Ram", Some("Z4D-HIGHMEM-HIGHLSSD "))
            } else {
                ("Z4D-HIGHMEM-STANDARDLSSD Instance Core", "Z4D-HIGHMEM-STANDARDLSSD Instance Ram", Some("Z4D-HIGHMEM-STANDARDLSSD "))
            }
        }
        _ => return None,
    })
}

fn custom_stem(family: &str) -> Option<&'static str> {
    Some(match family {
        "n1" => "Custom Instance",
        "n2" => "N2 Custom Instance",
        "n2d" => "N2D AMD Custom Instance",
        "e2" => "E2 Custom Instance",
        "n4" => "N4 Custom Instance",
        "n4a" => "N4A Custom Instance",
        "n4d" => "N4D Custom Instance",
        "g2" => "G2 Custom Instance",
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pricing {
    OnDemand,
    Spot,
    Commit1Yr,
    Commit3Yr,
}

pub struct MachinePrice {
    pub cpu: Option<f64>,
    pub ram: Option<f64>,
    pub cpu_sku: Option<String>,
    pub ram_sku: Option<String>,
}

fn esc(s: &str) -> String {
    regex::escape(s)
}

/// Build SKU regexes for the CPU and RAM of a machine type.
pub fn sku_patterns(mt: &MachineType, pricing: Pricing) -> Option<(String, String)> {
    let (core, ram, commit) = family_skus(&mt.family, &mt.name)?;
    match pricing {
        Pricing::OnDemand | Pricing::Spot => {
            let prefix = if pricing == Pricing::Spot { "Spot Preemptible " } else { "" };
            if mt.custom {
                let stem = custom_stem(&mt.family)?;
                let stem_ram = if mt.family == "n2d" { "N2D AMD Custom Instance" } else { stem };
                Some((format!("^{prefix}{} Core running in", esc(stem)), format!("^{prefix}{} Ram running in", esc(stem_ram))))
            } else {
                Some((format!("^{prefix}{} running in", esc(core)), format!("^{prefix}{} running in", esc(ram))))
            }
        }
        Pricing::Commit1Yr | Pricing::Commit3Yr => {
            let years = if pricing == Pricing::Commit1Yr { "1" } else { "3" };
            if mt.family == "c2" {
                return Some((
                    format!("^Commitment: Compute optimized Core running in .* for {years} Year"),
                    format!("^Commitment: Compute optimized Ram running in .* for {years} Year"),
                ));
            }
            let c = commit?;
            Some((
                format!("^Commitment v1: {}Cpu in .* for {years} Year", esc(c)),
                format!("^Commitment v1: {}Ram in .* for {years} Year", esc(c)),
            ))
        }
    }
}

/// Hourly price of one vCPU and one GiB of RAM for a machine type.
pub fn unit_prices(p: &Pricer, region: &str, mt: &MachineType, pricing: Pricing) -> MachinePrice {
    let usage = match pricing {
        Pricing::OnDemand => "OnDemand",
        Pricing::Spot => "Preemptible",
        Pricing::Commit1Yr => "Commit1Yr",
        Pricing::Commit3Yr => "Commit3Yr",
    };
    let Some((cpu_re, ram_re)) = sku_patterns(mt, pricing) else {
        return MachinePrice { cpu: None, ram: None, cpu_sku: None, ram_sku: None };
    };
    let cpu = p.find(Query::new(COMPUTE, region, &cpu_re).usage(usage).family("Compute"));
    let ram = p.find(Query::new(COMPUTE, region, &ram_re).usage(usage).family("Compute"));
    MachinePrice {
        cpu: cpu.as_ref().map(|x| x.unit_price()),
        ram: ram.as_ref().map(|x| x.unit_price()),
        cpu_sku: cpu.map(|x| x.sku_id.clone()),
        ram_sku: ram.map(|x| x.sku_id.clone()),
    }
}

/// Effective billed vCPUs for shared-core machine types.
pub fn billed_vcpus(mt: &MachineType) -> f64 {
    match mt.name.as_str() {
        "e2-micro" => 0.25,
        "e2-small" => 0.5,
        "e2-medium" => 1.0,
        _ => mt.vcpus,
    }
}

/// GPU SKU description stem for an accelerator type.
pub fn gpu_stem(accel: &str) -> Option<&'static str> {
    Some(match accel.rsplit('/').next().unwrap_or(accel) {
        "nvidia-tesla-t4" | "nvidia-tesla-t4-vws" => "Nvidia Tesla T4 GPU",
        "nvidia-tesla-v100" => "Nvidia Tesla V100 GPU",
        "nvidia-tesla-p100" | "nvidia-tesla-p100-vws" => "Nvidia Tesla P100 GPU",
        "nvidia-tesla-p4" | "nvidia-tesla-p4-vws" => "Nvidia Tesla P4 GPU",
        "nvidia-tesla-k80" => "Nvidia Tesla K80 GPU",
        "nvidia-tesla-a100" => "Nvidia Tesla A100 GPU",
        "nvidia-a100-80gb" => "Nvidia Tesla A100 80GB GPU",
        "nvidia-l4" | "nvidia-l4-vws" => "Nvidia L4 GPU",
        "nvidia-h100-80gb" => "Nvidia H100 80GB GPU",
        "nvidia-h100-mega-80gb" => "Nvidia H100 80GB Plus GPU",
        "nvidia-h200-141gb" => "H200 141GB GPU",
        _ => return None,
    })
}

/// GPU SKU regex for on-demand or Spot pricing.
pub fn gpu_pattern(accel: &str, spot: bool) -> Option<String> {
    let stem = regex::escape(gpu_stem(accel)?);
    Some(if spot {
        format!("^(Spot Preemptible )?{stem}( attached to Spot Preemptible VMs)? running in")
    } else {
        format!("^{stem} running in")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_types() {
        let m = parse("n2-standard-4").unwrap();
        assert_eq!((m.vcpus, m.memory_gb, m.family.as_str()), (4.0, 16.0, "n2"));
        let c = parse("n2-custom-6-20480").unwrap();
        assert!(c.custom && c.vcpus == 6.0 && c.memory_gb == 20.0);
        let n1 = parse("custom-2-4096").unwrap();
        assert_eq!(n1.family, "n1");
        let e = parse("n2-custom-2-32768-ext").unwrap();
        assert_eq!(e.extended_memory_gb, 16.0);
        let g = parse("a2-highgpu-1g").unwrap();
        assert_eq!(g.accelerators, vec![("nvidia-tesla-a100".to_string(), 1.0)]);
        assert!(parse("e2-micro").unwrap().shared_core);
    }
}
