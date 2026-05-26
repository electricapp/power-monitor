//! Combined sampler: power, CPU/GPU utilisation, and memory -- one line per second.

fn main() {
    let mut sampler = power_monitor::Sampler::new().expect("failed to open subsystems");

    println!(
        "{:<8}  {:>7}  {:>7}  {:>7}  {:>7}  {:>7}  {:>8}  {:>8}",
        "sys W", "cpu W", "gpu W", "ane W", "ecpu%", "pcpu%", "ram GB", "swap GB"
    );

    loop {
        // Blocks for 1 second, averaging 4 sub-windows internally.
        let m = sampler.get_metrics(1000);

        println!(
            "{:<8.2}  {:>7.2}  {:>7.2}  {:>7.2}  {:>7.1}  {:>7.1}  {:>8.2}  {:>8.2}",
            m.sys_power,
            m.cpu_power,
            m.gpu_power,
            m.ane_power,
            m.ecpu.utilization * 100.0,
            m.pcpu.utilization * 100.0,
            m.memory.used as f64 / 1e9,
            m.swap.used as f64 / 1e9,
        );
    }
}
