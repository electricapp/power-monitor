//! IOReport Energy Model: per-component watts over a 1-second window.

fn main() {
    let mut sampler = power_monitor::EnergySampler::new().expect("IOReport unavailable");

    // Allow the first interval to accumulate before sampling.
    std::thread::sleep(std::time::Duration::from_millis(1000));

    let r = sampler.sample();

    println!("interval : {:.0} ms", r.interval_ms);
    println!("ECPU     : {:.2} W", r.ecpu);
    println!("PCPU     : {:.2} W", r.pcpu);
    println!("CPU total: {:.2} W", r.cpu_total());
    println!("GPU      : {:.2} W", r.gpu);
    println!("ANE      : {:.2} W", r.ane);
    println!("DRAM     : {:.2} W", r.dram);
    println!("DISP     : {:.2} W", r.disp);
}
