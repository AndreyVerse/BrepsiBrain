//! Железо: чем считается модель и какие модели показывать.
//!
//! Тир считается по видеопамяти и бэкенду: CUDA/Vulkan — ускорение есть,
//! чистый процессор — только лёгкие модели. Детект кэшируется: DXGI и
//! `nvidia-smi` порождают процессы, а спрашивают их на каждый запрос статуса.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Unknown,
}

/// Чем реально ускоряется инференс на этой машине.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Cuda,
    /// AMD/Intel: ROCm в комплект не входит, работает Vulkan-бэкенд Ollama.
    Vulkan,
    Cpu,
}

/// Тир подбора каталога: пользователь видит модели не крупнее своего железа.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// CPU или карта без ускорения: только лёгкие модели.
    Minimal,
    /// 7–10.9 ГБ VRAM: 7–8B в Q4 (RX 580 8 ГБ).
    Low,
    /// 11–14.9 ГБ: 12–14B в Q4.
    Mid,
    /// 15 ГБ и больше: 27–32B в Q4.
    High,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Minimal => "minimal",
            Tier::Low => "low",
            Tier::Mid => "mid",
            Tier::High => "high",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hardware {
    pub vendor: Vendor,
    pub name: Option<String>,
    pub vram_mb: Option<u64>,
    pub ram_mb: u64,
    pub backend: Backend,
    pub tier: Tier,
    /// Пояснения для интерфейса: почему выбран этот тир.
    pub notes: Vec<String>,
}

const TIER_HIGH_VRAM_MB: u64 = 15_000;
const TIER_MID_VRAM_MB: u64 = 11_000;
const TIER_LOW_VRAM_MB: u64 = 7_000;

pub fn tier_for(vram_mb: Option<u64>, backend: Backend) -> Tier {
    match backend {
        Backend::Cpu => Tier::Minimal,
        Backend::Cuda | Backend::Vulkan => match vram_mb.unwrap_or(0) {
            vram if vram >= TIER_HIGH_VRAM_MB => Tier::High,
            vram if vram >= TIER_MID_VRAM_MB => Tier::Mid,
            vram if vram >= TIER_LOW_VRAM_MB => Tier::Low,
            _ => Tier::Minimal,
        },
    }
}

static CACHED: std::sync::OnceLock<Hardware> = std::sync::OnceLock::new();

/// Полный профиль железа; считается один раз на процесс.
pub fn detect() -> Hardware {
    CACHED.get_or_init(detect_uncached).clone()
}

fn detect_uncached() -> Hardware {
    let ram_mb = total_ram_mb().unwrap_or(0);
    if let Some(hardware) = detect_nvidia(ram_mb) {
        return hardware;
    }
    if let Some(mut hardware) = detect_dxgi(ram_mb) {
        if hardware.vendor == Vendor::Nvidia {
            hardware.notes.push(
                "nvidia-smi не ответил: CUDA может быть недоступна (обесточенная dGPU ноутбука или сбой драйвера).".into(),
            );
        }
        return hardware;
    }
    Hardware {
        vendor: Vendor::Unknown,
        name: None,
        vram_mb: None,
        ram_mb,
        backend: Backend::Cpu,
        tier: Tier::Minimal,
        notes: vec!["Дискретная видеокарта не найдена: модели считает процессор.".into()],
    }
}

/// NVIDIA: `nvidia-smi` даёт точный объём VRAM и подтверждает CUDA.
fn detect_nvidia(ram_mb: u64) -> Option<Hardware> {
    let mut command = Command::new("nvidia-smi");
    command.args(["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"]);
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().ok()?;
    // Висящий nvidia-smi — известная болезнь драйверов, а detect() кэшируется
    // навсегда: без дедлайна опрос железа встал бы намертво. Вывод — строка на
    // видеокарту, поэтому pipe не переполняется, пока мы ждём выход.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().ok()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    };
    if !status.success() {
        return None;
    }
    let mut stdout = Vec::new();
    child.stdout.take()?.read_to_end(&mut stdout).ok()?;
    let text = String::from_utf8_lossy(&stdout);
    // В паре «GT 710 + RTX 4070» строки идут по шине: берём самую ёмкую.
    let (name, vram_mb) = text
        .lines()
        .map(|line| {
            let parts: Vec<&str> = line.split(',').map(str::trim).collect();
            (
                parts.first().filter(|part| !part.is_empty()).map(|part| (*part).to_owned()),
                parts.get(1).and_then(|part| part.parse::<u64>().ok()),
            )
        })
        .max_by_key(|(_, vram)| vram.unwrap_or(0))?;
    let backend = Backend::Cuda;
    Some(Hardware {
        vendor: Vendor::Nvidia,
        name,
        vram_mb,
        ram_mb,
        backend,
        tier: tier_for(vram_mb, backend),
        notes: Vec::new(),
    })
}

/// AMD/Intel: DXGI отдаёт `DedicatedVideoMemory` без обрезки до 4 ГБ (в отличие
/// от `AdapterRAM` в WMI).
#[cfg(windows)]
fn detect_dxgi(ram_mb: u64) -> Option<Hardware> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};

    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1().ok()? };
    let mut best: Option<Hardware> = None;
    for index in 0..8u32 {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else { break };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else { continue };
        if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
            continue;
        }
        let name_len = desc.Description.iter().position(|ch| *ch == 0).unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..name_len]).trim().to_owned();
        let vram_mb = desc.DedicatedVideoMemory as u64 / (1024 * 1024);
        if vram_mb < 512 {
            continue;
        }
        let vendor = match desc.VendorId {
            0x10DE => Vendor::Nvidia,
            0x1002 | 0x1022 => Vendor::Amd,
            0x8086 => Vendor::Intel,
            _ => Vendor::Unknown,
        };
        let candidate = build(vendor, name, vram_mb, ram_mb);
        if best.as_ref().is_none_or(|current| candidate.vram_mb.unwrap_or(0) > current.vram_mb.unwrap_or(0)) {
            best = Some(candidate);
        }
    }
    best
}

#[cfg(not(windows))]
fn detect_dxgi(_ram_mb: u64) -> Option<Hardware> {
    None
}

/// Профиль карты по вендору и объёму: интел и AMD считают через Vulkan.
pub fn build(vendor: Vendor, name: String, vram_mb: u64, ram_mb: u64) -> Hardware {
    let mut notes = Vec::new();
    let backend = match vendor {
        Vendor::Nvidia => Backend::Cuda,
        Vendor::Amd => {
            if legacy_amd(&name) {
                notes.push(
                    "Карта AMD поколения Polaris/Vega: ROCm её не поддерживает, ускорение идёт через Vulkan-бэкенд Ollama.".into(),
                );
            } else {
                notes.push("Карта AMD: ускорение идёт через Vulkan-бэкенд Ollama.".into());
            }
            Backend::Vulkan
        }
        Vendor::Intel => {
            notes.push("Intel: ускорение возможно через Vulkan, видеопамять общая с системной.".into());
            Backend::Vulkan
        }
        Vendor::Unknown => Backend::Cpu,
    };
    Hardware {
        vendor,
        name: Some(name),
        vram_mb: Some(vram_mb),
        ram_mb,
        backend,
        tier: tier_for(Some(vram_mb), backend),
        notes,
    }
}

/// RX 400/500 и Vega: gfx803/gfx900 вне актуального ROCm.
fn legacy_amd(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("rx 4") || lower.contains("rx 5") || lower.contains("vega") || lower.contains("polaris")
}

fn total_ram_mb() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
        return Some(status.ullTotalPhys / (1024 * 1024));
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Свободное место на диске, байты. Путь может ещё не существовать — берётся
/// ближайший существующий родитель.
pub fn free_space_bytes(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

        let mut current = path.to_path_buf();
        while !current.exists() {
            current = current.parent()?.to_path_buf();
        }
        let canonical = current.canonicalize().unwrap_or(current);
        let text = canonical.to_string_lossy();
        // `\\?\UNC\server\share` после срезания `\\?\` давал невалидный путь,
        // и проверка места на сетевом диске тихо отключалась.
        let mut narrowed = match text.strip_prefix(r"\\?\UNC\") {
            Some(unc) => format!(r"\\{unc}"),
            None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned(),
        };
        if !narrowed.ends_with('\\') && !narrowed.ends_with('/') {
            narrowed.push('\\');
        }
        let wide: Vec<u16> = narrowed.encode_utf16().chain(std::iter::once(0)).collect();
        let mut free = 0u64;
        let ok = unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None) };
        return ok.is_ok().then_some(free);
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_thresholds() {
        assert_eq!(tier_for(Some(16_303), Backend::Cuda), Tier::High);
        assert_eq!(tier_for(Some(12_288), Backend::Vulkan), Tier::Mid);
        assert_eq!(tier_for(Some(8_192), Backend::Vulkan), Tier::Low);
        assert_eq!(tier_for(Some(4_096), Backend::Cuda), Tier::Minimal);
        assert_eq!(tier_for(None, Backend::Cpu), Tier::Minimal);
        // Большая карта без ускорения считается процессором.
        assert_eq!(tier_for(Some(24_576), Backend::Cpu), Tier::Minimal);
    }

    #[test]
    fn rx_580_is_low_tier_with_vulkan() {
        let hardware = build(Vendor::Amd, "Radeon RX 580 Series".into(), 8_192, 32_768);
        assert_eq!(hardware.backend, Backend::Vulkan);
        assert_eq!(hardware.tier, Tier::Low);
        assert!(hardware.notes.iter().any(|note| note.contains("Vulkan")));
    }

    #[test]
    fn nvidia_and_intel_backends() {
        let nvidia = build(Vendor::Nvidia, "NVIDIA GeForce RTX 3060".into(), 12_288, 32_768);
        assert_eq!((nvidia.backend, nvidia.tier), (Backend::Cuda, Tier::Mid));
        let intel = build(Vendor::Intel, "Intel(R) Arc(TM) A750 Graphics".into(), 8_192, 32_768);
        assert_eq!((intel.backend, intel.tier), (Backend::Vulkan, Tier::Low));
    }

    #[test]
    fn free_space_of_existing_path_is_positive() {
        let dir = std::env::current_dir().unwrap();
        assert!(free_space_bytes(&dir).is_some_and(|bytes| bytes > 0));
        let nested = dir.join("no-such-dir-yet").join("deeper");
        assert!(free_space_bytes(&nested).is_some());
    }
}
