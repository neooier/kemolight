use std::{mem, process::Command, ptr};

use std::os::windows::process::CommandExt;

use windows_sys::{
    core::BOOL,
    Win32::{
        Devices::Display::{
            DestroyPhysicalMonitor, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
            GetPhysicalMonitorsFromHMONITOR, SetMonitorBrightness, PHYSICAL_MONITOR,
        },
        Foundation::{HANDLE, LPARAM, RECT},
        Graphics::Gdi::{
            EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, DISPLAY_DEVICEW, HDC,
            HMONITOR, MONITORINFOEXW,
        },
    },
};

use super::{
    clamp_percent, BrightnessBackend, BrightnessError, BrightnessValue, DisplayId, DisplayInfo,
    DisplayKind, Result,
};

const ID_PREFIX: &str = "windows-ddcci:";
const WMI_ID_PREFIX: &str = "windows-wmi:";
const POWERSHELL: &str = "powershell.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct WindowsBrightnessBackend;

impl BrightnessBackend for WindowsBrightnessBackend {
    fn list_displays(&self) -> Result<Vec<DisplayInfo>> {
        let mut displays = Vec::new();
        let mut first_error = None;

        match list_wmi_displays() {
            Ok(mut detected) => displays.append(&mut detected),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
        let internal_hardware_ids = wmi_display_hardware_ids(&displays);

        let monitors = match enumerate_physical_monitors() {
            Ok(monitors) => monitors,
            Err(error) => {
                if displays.is_empty() {
                    return Err(error);
                }
                first_error.get_or_insert(error);
                Vec::new()
            }
        };

        for monitor in monitors {
            if monitor.matches_hardware_ids(&internal_hardware_ids) {
                continue;
            }

            // A successful read already establishes brightness support. Querying
            // all capabilities can take seconds even on readable monitors.
            let current = monitor.get_brightness().ok().map(|value| value.percent());

            displays.push(DisplayInfo {
                id: monitor.id(),
                name: monitor.name(),
                kind: DisplayKind::ExternalDdcCi,
                current,
                max: 100,
                supported: current.is_some(),
            });
        }

        if displays.is_empty() {
            if let Some(error) = first_error {
                return Err(error);
            }
        }

        Ok(displays)
    }

    fn get_brightness(&self, display_id: &DisplayId) -> Result<BrightnessValue> {
        if display_id.as_str().starts_with(WMI_ID_PREFIX) {
            return wmi_get_brightness(display_id);
        }

        find_monitor(display_id)?.get_brightness()
    }

    fn set_brightness(&self, display_id: &DisplayId, value: u32) -> Result<()> {
        if display_id.as_str().starts_with(WMI_ID_PREFIX) {
            return wmi_set_brightness(display_id, value);
        }

        find_monitor(display_id)?.set_brightness(value)
    }
}

fn list_wmi_displays() -> Result<Vec<DisplayInfo>> {
    let output = powershell_output(
        r#"$items = @(Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightness -ErrorAction Stop | Where-Object { $_.Active })
for ($i = 0; $i -lt $items.Count; $i++) {
    $item = $items[$i]
    "$i|$($item.CurrentBrightness)|$($item.InstanceName)"
}"#,
    )?;

    Ok(parse_wmi_list(&output))
}

fn wmi_display_hardware_ids(displays: &[DisplayInfo]) -> Vec<String> {
    displays
        .iter()
        .filter_map(|display| {
            display
                .name
                .strip_prefix("Internal Display ")
                .map(normalize_hardware_id)
        })
        .collect()
}

fn wmi_get_brightness(display_id: &DisplayId) -> Result<BrightnessValue> {
    let instance = wmi_instance_name(display_id)?.replace('\'', "''");
    let script = format!(
        r#"$items = @(Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightness -ErrorAction Stop | Where-Object {{ $_.Active -and $_.InstanceName -eq '{instance}' }})
if ($items.Count -eq 0) {{ exit 2 }}
$items[0].CurrentBrightness"#
    );
    let output = powershell_output(&script)?;
    let current = output
        .lines()
        .find_map(|line| line.trim().parse::<u32>().ok())
        .ok_or_else(|| {
            BrightnessError::BackendUnavailable("could not parse WMI brightness".into())
        })?;

    Ok(BrightnessValue {
        current: clamp_percent(current),
        max: 100,
    })
}

fn wmi_set_brightness(display_id: &DisplayId, value: u32) -> Result<()> {
    let instance = wmi_instance_name(display_id)?.replace('\'', "''");
    let value = clamp_percent(value);
    let script = format!(
        r#"$brightness = {value}
$brightnessItems = @(Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightness -ErrorAction Stop | Where-Object {{ $_.Active -and $_.InstanceName -eq '{instance}' }})
if ($brightnessItems.Count -eq 0) {{ exit 2 }}
$instanceName = $brightnessItems[0].InstanceName
$methodItems = @(Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightnessMethods -ErrorAction Stop | Where-Object {{ $_.InstanceName -eq $instanceName }})
if ($methodItems.Count -eq 0) {{ exit 3 }}
Invoke-CimMethod -InputObject $methodItems[0] -MethodName WmiSetBrightness -Arguments @{{ Timeout = 1; Brightness = $brightness }} | Out-Null"#
    );
    powershell_output(&script).map(|_| ())
}

fn wmi_instance_name(display_id: &DisplayId) -> Result<&str> {
    display_id
        .as_str()
        .strip_prefix(WMI_ID_PREFIX)
        .filter(|value| hardware_id_from_device_instance(value).is_some())
        .ok_or_else(|| BrightnessError::DisplayNotFound(display_id.clone()))
}

fn powershell_output(script: &str) -> Result<String> {
    let output = Command::new(POWERSHELL)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                BrightnessError::CommandNotFound("powershell.exe".to_string())
            } else {
                BrightnessError::from(err)
            }
        })?;

    if !output.status.success() {
        let mut text = String::from_utf8_lossy(&output.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        let message = text.trim();
        return Err(BrightnessError::BackendUnavailable(if message.is_empty() {
            "Windows WMI brightness command failed".to_string()
        } else {
            message.to_string()
        }));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn parse_wmi_list(output: &str) -> Vec<DisplayInfo> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(3, '|');
            let index = parts.next()?.parse::<usize>().ok()?;
            let current = parts.next()?.parse::<u32>().ok().map(clamp_percent);
            let instance_name = parts.next().unwrap_or_default().trim();
            if instance_name.is_empty() {
                return None;
            }
            Some(DisplayInfo {
                id: DisplayId::new(format!(
                    "{WMI_ID_PREFIX}{}",
                    instance_name.to_ascii_uppercase()
                )),
                name: wmi_display_name(index, instance_name),
                kind: DisplayKind::InternalBacklight,
                current,
                max: 100,
                supported: current.is_some(),
            })
        })
        .collect()
}

fn wmi_display_name(index: usize, instance_name: &str) -> String {
    let label = hardware_id_from_device_instance(instance_name);

    match label.as_deref() {
        Some(label) => format!("Internal Display {label}"),
        None => format!("Internal Display {}", index + 1),
    }
}

fn hardware_id_from_device_instance(value: &str) -> Option<String> {
    let value = value.trim();
    let mut parts = value.split('\\');
    let prefix = parts.next()?;
    if !matches!(prefix, "DISPLAY" | "MONITOR") {
        return None;
    }

    parts
        .next()
        .filter(|id| !id.is_empty())
        .map(normalize_hardware_id)
}

fn normalize_hardware_id(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_uppercase())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MonitorAddress {
    logical_index: usize,
    physical_index: usize,
}

struct PhysicalMonitor {
    address: MonitorAddress,
    handle: HANDLE,
    description: String,
    hardware_id: Option<String>,
    device_instance: Option<String>,
}

impl PhysicalMonitor {
    fn id(&self) -> DisplayId {
        let device = self
            .device_instance
            .clone()
            .unwrap_or_else(|| format!("temporary:{}", self.address.logical_index));
        DisplayId::new(format!(
            "{ID_PREFIX}{device}:{}",
            self.address.physical_index
        ))
    }

    fn name(&self) -> String {
        if self.description.is_empty() {
            format!(
                "DDC/CI Display {}.{}",
                self.address.logical_index + 1,
                self.address.physical_index + 1
            )
        } else {
            self.description.clone()
        }
    }

    fn matches_hardware_ids(&self, hardware_ids: &[String]) -> bool {
        if hardware_ids.is_empty() {
            return false;
        }

        if let Some(hardware_id) = self.hardware_id.as_ref() {
            return hardware_ids.iter().any(|id| id == hardware_id);
        }

        let description = normalize_hardware_id(&self.description);
        hardware_ids
            .iter()
            .any(|id| !id.is_empty() && description.contains(id))
    }

    fn get_brightness(&self) -> Result<BrightnessValue> {
        let mut min = 0;
        let mut current = 0;
        let mut max = 0;
        let ok = unsafe { GetMonitorBrightness(self.handle, &mut min, &mut current, &mut max) };
        if ok == 0 || max <= min {
            return Err(ddcci_error("failed to read monitor brightness"));
        }

        Ok(BrightnessValue {
            current: raw_to_percent(current, min, max),
            max: 100,
        })
    }

    fn set_brightness(&self, value: u32) -> Result<()> {
        let (min, max) = self.raw_range()?;
        let raw = percent_to_raw(clamp_percent(value), min, max);
        let ok = unsafe { SetMonitorBrightness(self.handle, raw) };
        if ok == 0 {
            return Err(ddcci_error("failed to set monitor brightness"));
        }

        Ok(())
    }

    fn raw_range(&self) -> Result<(u32, u32)> {
        let mut min = 0;
        let mut current = 0;
        let mut max = 0;
        let ok = unsafe { GetMonitorBrightness(self.handle, &mut min, &mut current, &mut max) };
        if ok == 0 || max <= min {
            return Err(ddcci_error("failed to read monitor brightness range"));
        }

        Ok((min, max))
    }
}

impl Drop for PhysicalMonitor {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }

        unsafe {
            DestroyPhysicalMonitor(self.handle);
        }
    }
}

struct PhysicalMonitorArray {
    monitors: Vec<PHYSICAL_MONITOR>,
}

impl PhysicalMonitorArray {
    fn from_hmonitor(hmonitor: HMONITOR) -> Result<Self> {
        let mut count = 0;
        let ok = unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut count) };
        if ok == 0 {
            return Err(ddcci_error("failed to count physical monitors"));
        }
        if count == 0 {
            return Ok(Self {
                monitors: Vec::new(),
            });
        }

        let mut array = Self {
            monitors: vec![PHYSICAL_MONITOR::default(); count as usize],
        };
        let ok = unsafe {
            GetPhysicalMonitorsFromHMONITOR(hmonitor, count, array.monitors.as_mut_ptr())
        };
        if ok == 0 {
            return Err(ddcci_error("failed to get physical monitors"));
        }

        Ok(array)
    }
}

impl Drop for PhysicalMonitorArray {
    fn drop(&mut self) {
        for monitor in &self.monitors {
            if !monitor.hPhysicalMonitor.is_null() {
                unsafe { DestroyPhysicalMonitor(monitor.hPhysicalMonitor) };
            }
        }
    }
}

fn enumerate_logical_monitors() -> Result<Vec<HMONITOR>> {
    let mut hmonitors = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(enum_monitor),
            &mut hmonitors as *mut Vec<HMONITOR> as LPARAM,
        )
    };
    if ok == 0 {
        return Err(ddcci_error("failed to enumerate display monitors"));
    }
    Ok(hmonitors)
}

fn enumerate_physical_monitors() -> Result<Vec<PhysicalMonitor>> {
    let mut monitors = Vec::new();
    for (logical_index, hmonitor) in enumerate_logical_monitors()?.into_iter().enumerate() {
        let device_instance = monitor_device_instance(hmonitor);
        let hardware_id = device_instance.as_ref().and_then(|instance| {
            hardware_id_from_device_instance(
                &instance.trim_start_matches(r"\\?\").replace('#', "\\"),
            )
        });
        let mut physical_monitors = PhysicalMonitorArray::from_hmonitor(hmonitor)?;
        for (physical_index, physical_monitor) in physical_monitors.monitors.iter_mut().enumerate()
        {
            let description = physical_monitor.szPhysicalMonitorDescription;
            let monitor = PhysicalMonitor {
                address: MonitorAddress {
                    logical_index,
                    physical_index,
                },
                description: wide_to_string(&description),
                hardware_id: hardware_id.clone(),
                device_instance: device_instance.clone(),
                handle: physical_monitor.hPhysicalMonitor,
            };
            // The individual monitor now owns this handle. The array still owns its buffer.
            physical_monitor.hPhysicalMonitor = ptr::null_mut();
            monitors.push(monitor);
        }
    }

    Ok(monitors)
}

unsafe extern "system" fn enum_monitor(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let monitors = &mut *(data as *mut Vec<HMONITOR>);
    monitors.push(hmonitor);
    1
}

fn monitor_device_instance(hmonitor: HMONITOR) -> Option<String> {
    monitor_device_path(hmonitor, false)
}

fn monitor_device_path(hmonitor: HMONITOR, require_unique: bool) -> Option<String> {
    let mut monitor_info = MONITORINFOEXW::default();
    monitor_info.monitorInfo.cbSize = mem::size_of::<MONITORINFOEXW>() as u32;

    let ok = unsafe { GetMonitorInfoW(hmonitor, &mut monitor_info as *mut _ as *mut _) };
    if ok == 0 {
        return None;
    }

    let mut device = DISPLAY_DEVICEW {
        cb: mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    // EDD_GET_DEVICE_INTERFACE_NAME identifies this monitor independently of
    // the current logical display enumeration order.
    let ok = unsafe { EnumDisplayDevicesW(monitor_info.szDevice.as_ptr(), 0, &mut device, 1) };
    if ok == 0 {
        return None;
    }

    let instance = wide_to_string(&device.DeviceID).to_ascii_uppercase();
    // In clone mode a logical monitor can be shared by multiple physical screens.
    device.cb = mem::size_of::<DISPLAY_DEVICEW>() as u32;
    if require_unique
        && unsafe { EnumDisplayDevicesW(monitor_info.szDevice.as_ptr(), 1, &mut device, 1) } != 0
    {
        return None;
    }
    (!instance.is_empty()).then_some(instance)
}

pub(crate) fn monitor_targets(displays: &[DisplayInfo]) -> Vec<(isize, DisplayId)> {
    enumerate_logical_monitors()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|monitor| {
            let path = monitor_device_path(monitor, true)?;
            display_for_monitor(displays, &path).map(|id| (monitor as isize, id.clone()))
        })
        .collect()
}

fn display_for_monitor<'a>(displays: &'a [DisplayInfo], path: &str) -> Option<&'a DisplayId> {
    let ddcci_prefix = format!("{ID_PREFIX}{path}:");
    // A device interface path ends in a class GUID. WMI uses the PnP instance
    // name for the same device, followed by a provider instance index.
    let wmi_instance = path
        .strip_prefix(r"\\?\")
        .and_then(|path| path.rsplit_once("#{"))
        .map(|(instance, _)| instance.replace('#', "\\"));
    let mut matches = displays.iter().filter(|display| {
        let id = display.id.as_str();
        id.starts_with(&ddcci_prefix)
            || wmi_instance.as_deref().is_some_and(|instance| {
                id.strip_prefix(WMI_ID_PREFIX)
                    .and_then(|id| id.rsplit_once('_'))
                    .is_some_and(|(device, index)| {
                        device == instance && index.parse::<u32>().is_ok()
                    })
            })
    });
    let display = matches.next()?;
    (display.supported && matches.next().is_none()).then_some(&display.id)
}

fn find_monitor(display_id: &DisplayId) -> Result<PhysicalMonitor> {
    enumerate_physical_monitors()?
        .into_iter()
        .find(|monitor| monitor.id() == *display_id)
        .ok_or_else(|| BrightnessError::DisplayNotFound(display_id.clone()))
}

fn raw_to_percent(current: u32, min: u32, max: u32) -> u32 {
    if max <= min {
        return 0;
    }
    ((current.saturating_sub(min).saturating_mul(100) + ((max - min) / 2)) / (max - min)).min(100)
}

fn percent_to_raw(percent: u32, min: u32, max: u32) -> u32 {
    min + (u64::from(clamp_percent(percent)) * u64::from(max - min) / 100) as u32
}

fn wide_to_string(value: &[u16]) -> String {
    let len = value.iter().position(|ch| *ch == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..len]).trim().to_string()
}

fn ddcci_error(context: &str) -> BrightnessError {
    let os_error = std::io::Error::last_os_error();
    let message = match os_error.raw_os_error() {
        Some(code) if code != 0 => format!("{context}: {os_error}"),
        _ => context.to_string(),
    };

    BrightnessError::BackendUnavailable(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_id_survives_logical_display_reordering() {
        let mut monitor = PhysicalMonitor {
            address: MonitorAddress {
                logical_index: 2,
                physical_index: 0,
            },
            handle: ptr::null_mut(),
            description: "Monitor".into(),
            hardware_id: None,
            device_instance: Some(r"\\?\DISPLAY#AOCB422#PORT1".into()),
        };
        let id = monitor.id();
        monitor.address.logical_index = 0;
        assert_eq!(monitor.id(), id);
        monitor.device_instance = Some(r"\\?\DISPLAY#AOCB422#PORT2".into());
        assert_ne!(monitor.id(), id);
    }

    #[test]
    fn parses_wmi_brightness_displays() {
        let output = r#"0|60|DISPLAY\SDC420B\4&1a2d752c&0&UID8388688_0
1|35|DISPLAY\ABC1234\5&11111111&0&UID1234567_0
"#;

        let displays = parse_wmi_list(output);
        assert_eq!(displays.len(), 2);
        assert_eq!(
            displays[0].id.as_str(),
            r"windows-wmi:DISPLAY\SDC420B\4&1A2D752C&0&UID8388688_0"
        );
        assert_eq!(displays[0].name, "Internal Display SDC420B");
        assert_eq!(displays[0].kind, DisplayKind::InternalBacklight);
        assert_eq!(displays[0].current, Some(60));
        assert!(displays[0].supported);
    }

    #[test]
    fn wmi_identity_survives_enumeration_reordering() -> Result<()> {
        let original = parse_wmi_list(r"0|60|DISPLAY\SDC420B\port1");
        let reordered = parse_wmi_list(r"1|60|DISPLAY\SDC420B\port1");
        assert_eq!(original[0].id, reordered[0].id);
        assert_eq!(
            wmi_instance_name(&original[0].id)?,
            r"DISPLAY\SDC420B\PORT1"
        );
        assert!(wmi_instance_name(&DisplayId::new("windows-wmi:3")).is_err());
        Ok(())
    }

    #[test]
    fn wheel_targets_match_device_instances_and_skip_ambiguous_or_unsupported_screens() {
        let mut displays = parse_wmi_list("0|60|DISPLAY\\SAME123\\PORT1_0\n");
        let external_path = r"\\?\DISPLAY#SAME123#PORT2#{E6F07B5F-EE97-4A90-B076-33F57BF4EAA7}";
        let external_id = DisplayId::new(format!("{ID_PREFIX}{external_path}:0"));
        displays.push(DisplayInfo {
            id: external_id.clone(),
            name: "External".into(),
            kind: DisplayKind::ExternalDdcCi,
            current: Some(70),
            max: 100,
            supported: true,
        });

        let internal_path = r"\\?\DISPLAY#SAME123#PORT1#{E6F07B5F-EE97-4A90-B076-33F57BF4EAA7}";
        assert_eq!(
            display_for_monitor(&displays, internal_path),
            Some(&displays[0].id)
        );
        assert_eq!(
            display_for_monitor(&displays, external_path),
            Some(&external_id)
        );
        assert_eq!(
            display_for_monitor(&displays, r"\\?\DISPLAY#SAME123#PORT3#{GUID}"),
            None
        );

        displays[1].supported = false;
        assert_eq!(display_for_monitor(&displays, external_path), None);
        displays[1].supported = true;
        let mut second = displays[1].clone();
        second.id = DisplayId::new(format!("{ID_PREFIX}{external_path}:1"));
        displays.push(second);
        assert_eq!(display_for_monitor(&displays, external_path), None);
    }

    #[test]
    fn extracts_normalized_monitor_hardware_id() {
        assert_eq!(
            hardware_id_from_device_instance(r"DISPLAY\SDC420B\4&abc"),
            Some("SDC420B".to_string())
        );
        assert_eq!(
            hardware_id_from_device_instance(r"MONITOR\sdc420b\4&abc"),
            Some("SDC420B".to_string())
        );
        assert_eq!(hardware_id_from_device_instance(r"PCI\VEN_1234"), None);
    }

    #[test]
    fn physical_monitor_matches_wmi_hardware_id() {
        let monitor = PhysicalMonitor {
            address: MonitorAddress {
                logical_index: 0,
                physical_index: 0,
            },
            handle: std::ptr::null_mut(),
            description: "Generic PnP Monitor".to_string(),
            hardware_id: Some("SDC420B".to_string()),
            device_instance: None,
        };

        assert!(monitor.matches_hardware_ids(&["SDC420B".to_string()]));
        assert!(!monitor.matches_hardware_ids(&["DEL1234".to_string()]));
    }

    #[test]
    fn raw_percent_mapping_respects_monitor_range() {
        assert_eq!(raw_to_percent(10, 10, 110), 0);
        assert_eq!(raw_to_percent(60, 10, 110), 50);
        assert_eq!(raw_to_percent(120, 10, 110), 100);
        assert_eq!(percent_to_raw(50, 10, 110), 60);
        assert_eq!(percent_to_raw(150, 10, 110), 110);
    }

    #[test]
    fn wide_to_string_stops_at_nul() {
        assert_eq!(wide_to_string(&[65, 66, 0, 67]), "AB");
    }
}
