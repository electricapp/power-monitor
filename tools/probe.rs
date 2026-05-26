/// Probe SMC keys, IOReport channels, and DVFS frequency tables.
use power_monitor::{ChannelGroup, ChannelKind, EnergySampler, MultiGroupSampler, Smc, SocInfo};
use std::collections::HashMap;
use std::thread;
use std::time::Duration;

fn main() {
    // ── SoC info + DVFS tables ───────────────────────────────────────────────
    let soc = SocInfo::from_system();
    println!("Chip: {}  ({} GPU cores)", soc.chip_name, soc.gpu_cores);
    for (i, l) in soc.perf_levels.iter().enumerate() {
        println!(
            "  perflevel{i}: {} -- {} cores -- {} freq entries: {:?}",
            l.name,
            l.cores,
            l.freqs_mhz.len(),
            &l.freqs_mhz
        );
    }
    println!(
        "  GPU freqs ({} entries): {:?}",
        soc.gpu_freqs_mhz.len(),
        &soc.gpu_freqs_mhz
    );

    // Scan all AppleARMIODevice nodes for voltage-states properties to find M5 names
    println!();
    println!("Scanning AppleARMIODevice nodes for voltage-states* properties:");
    power_monitor::soc::probe_voltage_state_properties();
    println!();

    let smc = Smc::open().expect("failed to open SMC");

    // Known Apple Silicon SMC key prefixes:
    // T = Temperature, P = Power, V = Voltage, I = Current
    // F = Fan, C = CPU, G = GPU
    let prefixes = [
        // Power
        (b"PSTR", "System total power"),
        (b"PHPC", "HPC/P-cluster power (not GPU)"),
        (b"PDTR", "DC rail power (VD0R*ID0R, not DRAM)"),
        (b"PANT", "ANE power"),
        (b"PCPT", "CPU package power"),
        (b"PCPC", "CPU core power"),
        (b"PGTR", "GPU rail power"),
        (b"PSDC", "SoC power"),
        (b"PPBR", "Package base rail"),
        (b"PSTC", "System total continuous"),
        (b"PBUS", "Bus power"),
        // Temperature
        (b"Tp01", "CPU efficiency core 1 temp"),
        (b"Tp02", "CPU efficiency core 2 temp"),
        (b"Tp03", "CPU efficiency core 3 temp"),
        (b"Tp04", "CPU efficiency core 4 temp"),
        (b"Tp05", "CPU perf core 1 temp"),
        (b"Tp06", "CPU perf core 2 temp"),
        (b"Tp07", "CPU perf core 3 temp"),
        (b"Tp08", "CPU perf core 4 temp"),
        (b"Tp09", "CPU core 9 temp"),
        (b"Tp0a", "CPU core 10 temp"),
        (b"Tp0b", "CPU core 11 temp"),
        (b"Tp0c", "CPU core 12 temp"),
        (b"Tp0d", "CPU core 13 temp"),
        (b"Tp0e", "CPU core 14 temp"),
        (b"Tp0f", "CPU core 15 temp"),
        (b"Tp0g", "CPU core 16 temp"),
        (b"Tg0a", "GPU temp A"),
        (b"Tg0b", "GPU temp B"),
        (b"Tg0c", "GPU temp C"),
        (b"Tg0d", "GPU temp D"),
        (b"Tg0f", "GPU temp F"),
        (b"Tm0a", "Memory temp A"),
        (b"Tm0b", "Memory temp B"),
        (b"TaLP", "Airflow left temp"),
        (b"TaRP", "Airflow right temp"),
        (b"Ts0S", "SSD temp"),
        (b"Ts1S", "SSD temp 2"),
        (b"Tw0P", "Wireless temp"),
        (b"TW0P", "Wireless temp 2"),
        (b"TC0P", "CPU proximity temp"),
        (b"TC0c", "CPU core temp"),
        (b"TG0P", "GPU proximity temp"),
        (b"Tp0P", "CPU package temp"),
        (b"Tb0P", "Battery temp"),
        (b"TA0P", "Ambient temp"),
        (b"TA1P", "Ambient temp 2"),
        (b"Th1H", "Heatsink 1 temp"),
        (b"Th2H", "Heatsink 2 temp"),
        // Fan
        (b"F0Ac", "Fan 0 actual RPM"),
        (b"F0Tg", "Fan 0 target RPM"),
        (b"F0Mn", "Fan 0 min RPM"),
        (b"F0Mx", "Fan 0 max RPM"),
        (b"F1Ac", "Fan 1 actual RPM"),
        (b"F1Tg", "Fan 1 target RPM"),
        (b"F1Mn", "Fan 1 min RPM"),
        (b"F1Mx", "Fan 1 max RPM"),
        (b"FNum", "Number of fans"),
        // Voltage
        (b"VC0C", "CPU core voltage"),
        (b"VG0C", "GPU core voltage"),
        (b"VD0R", "DRAM voltage"),
        (b"VP0R", "12V rail voltage"),
        (b"Vp0C", "CPU package voltage"),
        // Current
        (b"IC0C", "CPU core current"),
        (b"IG0C", "GPU core current"),
        (b"ID0R", "DRAM current"),
        // Misc
        (b"BEMB", "Board temp"),
        (b"MBSe", "Model serial"),
        (b"BSFC", "Battery state"),
    ];

    println!("{:<6} {:<30} {:>10}  Type", "Key", "Description", "Value");
    println!("{}", "-".repeat(70));

    for (key, desc) in prefixes {
        if let Some(val) = smc.read_f32(key) {
            let key_str = std::str::from_utf8(key).unwrap_or("????");
            println!("{:<6} {:<30} {:>10.2}  flt", key_str, desc, val);
        }
    }

    // ── IOReport Energy Model ────────────────────────────────────────────────
    println!();
    println!("IOReport Energy Model (500 ms sample)");
    println!("{}", "-".repeat(70));

    println!("All Energy Model channels (500 ms sample):");
    println!(
        "  {:<32} {:>4}  {:>14}  {:>9}",
        "Channel", "Fmt", "RawDelta (mJ)", "W"
    );
    println!("  {}", "-".repeat(65));
    if let Some(mut em) = EnergySampler::new() {
        thread::sleep(Duration::from_millis(500));
        for ch in em.raw_channels() {
            println!(
                "  {:<32} {:>4}  {:>14}  {:>9.3}",
                ch.name, ch.fmt, ch.raw, ch.watts
            );
        }
    } else {
        println!("  [failed to open IOReport]");
    }

    println!();
    match EnergySampler::new() {
        None => println!("  [failed to open IOReport Energy Model]"),
        Some(mut em) => {
            thread::sleep(Duration::from_millis(500));
            let r = em.sample();
            println!("  GPU:  {:6.3} W", r.gpu);
            println!("  ECPU: {:6.3} W", r.ecpu);
            println!("  PCPU: {:6.3} W", r.pcpu);
            println!("  ANE:  {:6.3} W", r.ane);
            println!("  DRAM: {:6.3} W", r.dram);
            println!("  interval: {:.1} ms", r.interval_ms);
        }
    }

    // ── MultiGroupSampler diagnostic ─────────────────────────────────────────
    // This reveals whether CPU Stats / GPU Stats channels are arriving at all,
    // and whether IOReportStateGetCount returns > 0 for them (needed for util%).
    println!();
    println!("MultiGroupSampler — channel groups present (500 ms sample)");
    println!("{}", "-".repeat(70));

    match MultiGroupSampler::new() {
        None => println!("  [failed to open MultiGroupSampler]"),
        Some(mut multi) => {
            thread::sleep(Duration::from_millis(500));
            let interval_ms = multi.sample();
            let channels = multi.channels();

            // Count channels per group
            let mut by_group: HashMap<&'static str, Vec<String>> = HashMap::new();
            for ch in channels {
                by_group
                    .entry(ch.group.label())
                    .or_default()
                    .push(ch.channel.clone());
            }

            println!(
                "  interval: {interval_ms:.0} ms   total channels: {}",
                channels.len()
            );
            for (group, names) in &by_group {
                println!("  {group}: {} channels", names.len());
            }

            // Show all non-Energy-Model channels in detail (these are the util channels)
            println!();
            println!("  CPU/GPU Stats channels (need States kind + ticks > 0 for util%):");
            println!(
                "  {:<40} {:<12} {:>6}",
                "group/channel", "kind", "states/raw"
            );
            println!("  {}", "-".repeat(62));
            let mut found_stats = false;
            for ch in channels {
                if ch.group == ChannelGroup::EnergyModel {
                    continue;
                }
                found_stats = true;
                match &ch.kind {
                    ChannelKind::States(states) => {
                        let total_ticks: u64 = states.iter().map(|s| s.ticks).sum();
                        println!(
                            "  {:<40} {:<12} {:>6} states  ticks={}",
                            format!("{}/{}", ch.group.label(), ch.channel),
                            "States",
                            states.len(),
                            total_ticks
                        );
                        // Show first 3 states for inspection
                        for s in states.iter().take(3) {
                            println!("      {:>10} ticks  name={:?}", s.ticks, s.name);
                        }
                    }
                    ChannelKind::Energy(raw) => {
                        println!(
                            "  {:<40} {:<12} {:>6}",
                            format!("{}/{}", ch.group.label(), ch.channel),
                            "Energy(!)",
                            raw
                        );
                    }
                    _ => {}
                }
            }
            if !found_stats {
                println!("  [NO CPU/GPU Stats channels received — subscription or merge failed]");
            }
        }
    }
}
