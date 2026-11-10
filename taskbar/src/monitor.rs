//! System monitor: CPU, GPU and RAM load plus temperatures, for the bar.
//!
//! - CPU load: GetSystemTimes deltas. RAM: GlobalMemoryStatusEx.
//! - CPU/system temperature: ACPI thermal zones through the
//!   `\Thermal Zone Information(*)\Temperature` performance counter (no admin needed).
//! - GPU: NVIDIA's NVML (`nvml.dll`, ships with the driver) for load and temperature;
//!   otherwise load from the `\GPU Engine(*)\Utilization Percentage` counter
//!   (ported from Seelen UI's system_info module), without temperature.

use std::{collections::HashMap, ffi::c_void};

use windows::Win32::{
    Foundation::FILETIME,
    System::{
        LibraryLoader::{GetProcAddress, LoadLibraryW},
        Performance::{
            PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
            PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCollectQueryData,
            PdhGetFormattedCounterArrayW, PdhOpenQueryW,
        },
        SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
        Threading::GetSystemTimes,
    },
};
use windows::core::{PCWSTR, s, w};

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Stats {
    /// percent 0..100
    pub cpu: Option<f32>,
    /// °C, hottest thermal zone
    pub cpu_temp: Option<f32>,
    pub gpu: Option<f32>,
    pub gpu_temp: Option<f32>,
    pub ram: Option<f32>,
    pub ram_used_gb: f32,
    pub ram_total_gb: f32,
}


// ---------------------------------------------------------------------------
// NVML (NVIDIA)
// ---------------------------------------------------------------------------

type NvmlInit = unsafe extern "C" fn() -> i32;
type NvmlGetHandle = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
type NvmlGetTemp = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> i32;
type NvmlGetUtil = unsafe extern "C" fn(*mut c_void, *mut [u32; 2]) -> i32;

struct Nvml {
    device: *mut c_void,
    get_temp: NvmlGetTemp,
    get_util: NvmlGetUtil,
}

impl Nvml {
    fn load() -> Option<Nvml> {
        unsafe {
            let lib = LoadLibraryW(w!("nvml.dll")).ok()?;
            let init: NvmlInit = std::mem::transmute(GetProcAddress(lib, s!("nvmlInit_v2"))?);
            let get_handle: NvmlGetHandle =
                std::mem::transmute(GetProcAddress(lib, s!("nvmlDeviceGetHandleByIndex_v2"))?);
            let get_temp: NvmlGetTemp = std::mem::transmute(GetProcAddress(lib, s!("nvmlDeviceGetTemperature"))?);
            let get_util: NvmlGetUtil =
                std::mem::transmute(GetProcAddress(lib, s!("nvmlDeviceGetUtilizationRates"))?);
            if init() != 0 {
                return None;
            }
            let mut device = std::ptr::null_mut();
            if get_handle(0, &mut device) != 0 || device.is_null() {
                return None;
            }
            Some(Nvml { device, get_temp, get_util })
        }
    }

    /// (load %, temperature °C)
    fn read(&self) -> (Option<f32>, Option<f32>) {
        unsafe {
            let mut util = [0u32; 2];
            let load = ((self.get_util)(self.device, &mut util) == 0).then_some(util[0] as f32);
            let mut t = 0u32;
            // NVML_TEMPERATURE_GPU = 0
            let temp = ((self.get_temp)(self.device, 0, &mut t) == 0).then_some(t as f32);
            (load, temp)
        }
    }
}

// ---------------------------------------------------------------------------
// performance counters
// ---------------------------------------------------------------------------

/// Instance names (lowercase) and values of a wildcard counter.
fn read_counter(counter: PDH_HCOUNTER) -> Vec<(String, f64)> {
    for _ in 0..3 {
        let mut size = 0u32;
        let mut count = 0u32;
        let status = unsafe { PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, None) };
        if status != PDH_MORE_DATA {
            return Vec::new();
        }
        let item = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        let mut buf = vec![PDH_FMT_COUNTERVALUE_ITEM_W::default(); (size as usize).div_ceil(item)];
        let status = unsafe {
            PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(buf.as_mut_ptr()))
        };
        if status == PDH_MORE_DATA {
            continue; // instances changed in between
        }
        if status != PDH_CSTATUS_VALID_DATA {
            return Vec::new();
        }
        return buf[..count as usize]
            .iter()
            .filter(|i| i.FmtValue.CStatus == PDH_CSTATUS_VALID_DATA || i.FmtValue.CStatus == PDH_CSTATUS_NEW_DATA)
            .filter_map(|i| {
                let name = unsafe { i.szName.to_string() }.ok()?;
                Some((name.to_lowercase(), unsafe { i.FmtValue.Anonymous.doubleValue }))
            })
            .collect();
    }
    Vec::new()
}

/// `pid_1234_luid_..._phys_0_eng_0_engtype_3d` -> `luid_..._phys_0_eng_0`
fn engine_key(instance: &str) -> Option<&str> {
    let start = instance.find("luid_")?;
    let end = instance.find("_engtype_").unwrap_or(instance.len());
    instance.get(start..end)
}

fn filetime(t: FILETIME) -> u64 {
    (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64
}

pub struct Monitor {
    prev_cpu: Option<(u64, u64)>,
    query: PDH_HQUERY,
    thermal: Option<PDH_HCOUNTER>,
    gpu_engines: Option<PDH_HCOUNTER>,
    nvml: Option<Nvml>,
}

impl Monitor {
    pub fn new() -> Monitor {
        let mut query = PDH_HQUERY::default();
        let opened = unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } == PDH_CSTATUS_VALID_DATA;
        let add = |path: PCWSTR| -> Option<PDH_HCOUNTER> {
            if !opened {
                return None;
            }
            let mut c = PDH_HCOUNTER::default();
            (unsafe { PdhAddEnglishCounterW(query, path, 0, &mut c) } == PDH_CSTATUS_VALID_DATA).then_some(c)
        };
        let nvml = Nvml::load();
        let thermal = add(w!("\\Thermal Zone Information(*)\\Temperature"));
        let gpu_engines = if nvml.is_none() { add(w!("\\GPU Engine(*)\\Utilization Percentage")) } else { None };
        if opened {
            // rate counters need a first sample
            unsafe { PdhCollectQueryData(query) };
        }
        crate::log!(
            "monitor: nvml={} thermal={} gpu_counters={}",
            nvml.is_some(),
            thermal.is_some(),
            gpu_engines.is_some()
        );
        Monitor { prev_cpu: None, query, thermal, gpu_engines, nvml }
    }

    pub fn sample(&mut self) -> Stats {
        let mut st = Stats::default();

        // CPU: busy share of the time since the last sample
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        if unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.is_ok() {
            let idle = filetime(idle);
            let total = filetime(kernel) + filetime(user); // kernel time includes idle
            if let Some((pi, pt)) = self.prev_cpu {
                let (di, dt) = (idle.saturating_sub(pi), total.saturating_sub(pt));
                if dt > 0 {
                    st.cpu = Some(((dt - di.min(dt)) as f32 / dt as f32 * 100.0).clamp(0.0, 100.0));
                }
            }
            self.prev_cpu = Some((idle, total));
        }

        // RAM
        let mut mem = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if unsafe { GlobalMemoryStatusEx(&mut mem) }.is_ok() {
            st.ram = Some(mem.dwMemoryLoad as f32);
            let gb = |b: u64| b as f32 / (1024.0 * 1024.0 * 1024.0);
            st.ram_total_gb = gb(mem.ullTotalPhys);
            st.ram_used_gb = gb(mem.ullTotalPhys - mem.ullAvailPhys);
        }

        // counters: thermal zones (Kelvin) and, without NVML, GPU engines
        if self.thermal.is_some() || self.gpu_engines.is_some() {
            unsafe { PdhCollectQueryData(self.query) };
        }
        if let Some(c) = self.thermal {
            st.cpu_temp = read_counter(c)
                .into_iter()
                .map(|(_, k)| k as f32 - 273.15)
                .filter(|t| *t > 0.0 && *t < 150.0)
                .reduce(f32::max);
        }
        match &self.nvml {
            Some(nv) => (st.gpu, st.gpu_temp) = nv.read(),
            None => {
                if let Some(c) = self.gpu_engines {
                    // per engine, summed over processes; the GPU load is the busiest
                    // engine, like Task Manager (NPU engines are not the GPU)
                    let mut engines: HashMap<String, f64> = HashMap::new();
                    for (inst, v) in read_counter(c) {
                        if inst.contains("engtype_npu") {
                            continue;
                        }
                        if let Some(k) = engine_key(&inst) {
                            *engines.entry(k.to_string()).or_default() += v;
                        }
                    }
                    st.gpu = Some(engines.values().copied().fold(0.0, f64::max).min(100.0) as f32);
                }
            }
        }
        st
    }
}
