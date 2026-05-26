//! SMC snapshot: system power, GPU temperature, fan RPM, battery state.

fn main() {
    let mut smc = power_monitor::Smc::open().expect("failed to open SMC");
    let snap = smc.snapshot();

    println!("system power : {:.1} W", snap.power.system);
    println!("GPU temp     : {:.1} C", snap.thermal.gpu);
    println!("CPU P-cluster: {:.1} C", snap.thermal.cpu_performance);

    let fans = snap.fans;
    if fans.count == 0 {
        println!("fans         : none (fanless)");
    }
    for (i, fan) in fans.fans.iter().flatten().enumerate() {
        println!(
            "fan {}        : {:.0} RPM  (duty {:.0}%)",
            i,
            fan.rpm,
            fan.duty_cycle() * 100.0
        );
    }

    if snap.battery.is_present() {
        let pct = snap.battery.state_of_charge();
        let state = if snap.battery.is_charging() {
            "charging"
        } else {
            "discharging"
        };
        println!(
            "battery      : {:.0}%  {}  ({:.0} mAh remaining)",
            pct, state, snap.battery.remaining_mah
        );
    } else {
        println!("battery      : not present");
    }
}
