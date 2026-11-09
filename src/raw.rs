use std::error::Error;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError(pub &'static str);

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for ParseError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiskExtent {
    pub disk_number: u32,
    pub starting_offset: i64,
    pub extent_length: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionRecord {
    pub number: u32,
    pub starting_offset: i64,
    pub length: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriveLayout {
    pub partitions: Vec<PartitionRecord>,
    pub simple_data_only: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageDescriptor {
    pub bus_type: u32,
    pub vendor: String,
    pub product: String,
    pub serial: String,
}

pub fn needs_virtual_dependency_query(bus_type: u32) -> bool {
    // STORAGE_BUS_TYPE values from ntddstor.h.
    matches!(bus_type, 14 | 15) // BusTypeVirtual | BusTypeFileBackedVirtual
}

/// Reads the exact physical size from the fixed prefix of DISK_GEOMETRY_EX.
/// The 24-byte DISK_GEOMETRY is followed by a signed 64-bit DiskSize; the
/// variable-length partition/detection data after that prefix is optional.
pub fn parse_disk_geometry_capacity(bytes: &[u8]) -> Result<u64, ParseError> {
    const DISK_SIZE_OFFSET: usize = 24;
    const FIXED_PREFIX_SIZE: usize = DISK_SIZE_OFFSET + 8;
    if bytes.len() < FIXED_PREFIX_SIZE {
        return Err(ParseError("disk geometry buffer is truncated"));
    }
    let capacity = read_i64(bytes, DISK_SIZE_OFFSET)?;
    if capacity <= 0 {
        return Err(ParseError("disk geometry capacity is not positive"));
    }
    Ok(capacity as u64)
}

pub fn checked_dependency_entries_end(
    buffer_size: usize,
    entries_offset: usize,
    count: usize,
    entry_size: usize,
) -> Result<usize, ParseError> {
    let end = count
        .checked_mul(entry_size)
        .and_then(|bytes| entries_offset.checked_add(bytes))
        .ok_or(ParseError("virtual-disk dependency entry size overflow"))?;
    if end > buffer_size {
        return Err(ParseError("virtual-disk dependency entries are truncated"));
    }
    Ok(end)
}

pub fn parse_dependency_utf16(bytes: &[u8], offset: usize) -> Result<String, ParseError> {
    if offset % 2 != 0 || offset >= bytes.len() {
        return Err(ParseError(
            "virtual-disk dependency string offset is invalid",
        ));
    }
    let mut units = Vec::new();
    let mut cursor = offset;
    loop {
        let end = cursor
            .checked_add(2)
            .ok_or(ParseError("virtual-disk dependency string overflow"))?;
        let raw = bytes.get(cursor..end).ok_or(ParseError(
            "virtual-disk dependency string is not terminated",
        ))?;
        let unit = u16::from_le_bytes([raw[0], raw[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
        cursor = end;
    }
    String::from_utf16(&units)
        .map_err(|_| ParseError("virtual-disk dependency string contains invalid UTF-16"))
}

pub fn parse_multi_sz(values: &[u16]) -> Result<Vec<String>, ParseError> {
    if values.len() < 2 || values[values.len() - 2..] != [0, 0] {
        return Err(ParseError("MULTI_SZ is not double-NUL terminated"));
    }
    let mut result = Vec::new();
    let mut start = 0;
    while start < values.len() - 1 {
        let relative_end = values[start..]
            .iter()
            .position(|value| *value == 0)
            .ok_or(ParseError("MULTI_SZ item is not terminated"))?;
        if relative_end == 0 {
            break;
        }
        let end = start + relative_end;
        result.push(
            String::from_utf16(&values[start..end])
                .map_err(|_| ParseError("MULTI_SZ contains invalid UTF-16"))?,
        );
        start = end + 1;
    }
    Ok(result)
}

/// Parses the Win64 VOLUME_DISK_EXTENTS layout. The Windows backend is x64-only
/// in this first release and rejects oversized or malformed extent arrays.
pub fn parse_disk_extents(bytes: &[u8]) -> Result<Vec<DiskExtent>, ParseError> {
    const HEADER_SIZE: usize = 8;
    const EXTENT_SIZE: usize = 24;
    const MAX_EXTENTS: usize = 1_024;
    if bytes.len() < HEADER_SIZE {
        return Err(ParseError("volume extent buffer is too short"));
    }
    let count = read_u32(bytes, 0)? as usize;
    if count == 0 || count > MAX_EXTENTS {
        return Err(ParseError("volume extent count is invalid"));
    }
    let required = HEADER_SIZE
        .checked_add(
            count
                .checked_mul(EXTENT_SIZE)
                .ok_or(ParseError("volume extent size overflow"))?,
        )
        .ok_or(ParseError("volume extent size overflow"))?;
    if bytes.len() < required {
        return Err(ParseError("volume extent buffer is truncated"));
    }

    let mut extents = Vec::with_capacity(count);
    for index in 0..count {
        let offset = HEADER_SIZE + index * EXTENT_SIZE;
        let extent = DiskExtent {
            disk_number: read_u32(bytes, offset)?,
            starting_offset: read_i64(bytes, offset + 8)?,
            extent_length: read_i64(bytes, offset + 16)?,
        };
        if extent.starting_offset < 0 || extent.extent_length <= 0 {
            return Err(ParseError("volume extent has an invalid range"));
        }
        extents.push(extent);
    }
    Ok(extents)
}

/// Parses the Win64 `DRIVE_LAYOUT_INFORMATION_EX` layout used by the only
/// supported Windows target. Unsupported, boot, service, dynamic, or RAW
/// partition tables are returned as `simple_data_only = false` so policy can
/// list but never eject them.
pub fn parse_drive_layout(bytes: &[u8]) -> Result<DriveLayout, ParseError> {
    const HEADER_SIZE: usize = 48;
    const ENTRY_SIZE: usize = 144;
    const MAX_PARTITIONS: usize = 1_024;
    const PARTITION_STYLE_MBR: u32 = 0;
    const PARTITION_STYLE_GPT: u32 = 1;
    const BASIC_DATA_GUID: [u8; 16] = [
        0xa2, 0xa0, 0xd0, 0xeb, 0xe5, 0xb9, 0x33, 0x44, 0x87, 0xc0, 0x68, 0xb6, 0xb7, 0x26, 0x99,
        0xc7,
    ];
    const SIMPLE_MBR_TYPES: [u8; 7] = [0x01, 0x04, 0x06, 0x07, 0x0b, 0x0c, 0x0e];

    if bytes.len() < HEADER_SIZE {
        return Err(ParseError("drive layout buffer is too short"));
    }
    let style = read_u32(bytes, 0)?;
    let count = read_u32(bytes, 4)? as usize;
    if count > MAX_PARTITIONS {
        return Err(ParseError("drive partition count is invalid"));
    }
    let required = HEADER_SIZE
        .checked_add(
            count
                .checked_mul(ENTRY_SIZE)
                .ok_or(ParseError("drive layout size overflow"))?,
        )
        .ok_or(ParseError("drive layout size overflow"))?;
    if bytes.len() < required {
        return Err(ParseError("drive layout buffer is truncated"));
    }

    let mut simple_data_only = matches!(style, PARTITION_STYLE_MBR | PARTITION_STYLE_GPT);
    let mut partitions = Vec::with_capacity(count);
    for index in 0..count {
        let offset = HEADER_SIZE + index * ENTRY_SIZE;
        let entry_style = read_u32(bytes, offset)?;
        let starting_offset = read_i64(bytes, offset + 8)?;
        let length = read_i64(bytes, offset + 16)?;
        let number = read_u32(bytes, offset + 24)?;
        let is_service = *bytes
            .get(offset + 29)
            .ok_or(ParseError("drive layout entry is truncated"))?
            != 0;
        let unused_mbr = style == PARTITION_STYLE_MBR && bytes[offset + 32] == 0;
        if unused_mbr {
            continue;
        }
        if entry_style != style || number == 0 || starting_offset < 0 || length <= 0 {
            simple_data_only = false;
        }
        match style {
            PARTITION_STYLE_MBR => {
                let partition_type = bytes[offset + 32];
                let boot = bytes[offset + 33] != 0;
                let recognized = bytes[offset + 34] != 0;
                if !SIMPLE_MBR_TYPES.contains(&partition_type) || boot || !recognized || is_service
                {
                    simple_data_only = false;
                }
            }
            PARTITION_STYLE_GPT => {
                let partition_type = bytes
                    .get(offset + 32..offset + 48)
                    .ok_or(ParseError("GPT partition type is truncated"))?;
                let attributes = read_u64(bytes, offset + 64)?;
                if partition_type != BASIC_DATA_GUID || attributes != 0 || is_service {
                    simple_data_only = false;
                }
            }
            _ => simple_data_only = false,
        }
        partitions.push(PartitionRecord {
            number,
            starting_offset,
            length,
        });
    }
    if partitions.is_empty() {
        simple_data_only = false;
    }
    Ok(DriveLayout {
        partitions,
        simple_data_only,
    })
}

pub fn parse_storage_descriptor(bytes: &[u8]) -> Result<StorageDescriptor, ParseError> {
    const FIXED_SIZE: usize = 36;
    if bytes.len() < FIXED_SIZE {
        return Err(ParseError("storage descriptor is too short"));
    }
    let declared_size = read_u32(bytes, 4)? as usize;
    if declared_size < FIXED_SIZE || declared_size > bytes.len() {
        return Err(ParseError("storage descriptor size is invalid"));
    }
    let bounded = &bytes[..declared_size];
    Ok(StorageDescriptor {
        bus_type: read_u32(bounded, 28)?,
        vendor: read_c_string(bounded, read_u32(bounded, 12)? as usize)?,
        product: read_c_string(bounded, read_u32(bounded, 16)? as usize)?,
        serial: read_c_string(bounded, read_u32(bounded, 24)? as usize)?,
    })
}

fn read_c_string(bytes: &[u8], offset: usize) -> Result<String, ParseError> {
    if offset == 0 {
        return Ok(String::new());
    }
    if offset >= bytes.len() {
        return Err(ParseError("storage descriptor string offset is invalid"));
    }
    let end = bytes[offset..]
        .iter()
        .position(|byte| *byte == 0)
        .map(|relative| offset + relative)
        .ok_or(ParseError("storage descriptor string is not terminated"))?;
    Ok(std::str::from_utf8(&bytes[offset..end])
        .map_err(|_| ParseError("storage descriptor string is not UTF-8/ASCII"))?
        .trim()
        .to_owned())
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ParseError> {
    let end = offset
        .checked_add(4)
        .ok_or(ParseError("buffer offset overflow"))?;
    let value = bytes
        .get(offset..end)
        .ok_or(ParseError("buffer is truncated"))?;
    Ok(u32::from_le_bytes(value.try_into().expect("four bytes")))
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, ParseError> {
    let end = offset
        .checked_add(8)
        .ok_or(ParseError("buffer offset overflow"))?;
    let value = bytes
        .get(offset..end)
        .ok_or(ParseError("buffer is truncated"))?;
    Ok(i64::from_le_bytes(value.try_into().expect("eight bytes")))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ParseError> {
    let end = offset
        .checked_add(8)
        .ok_or(ParseError("buffer offset overflow"))?;
    let value = bytes
        .get(offset..end)
        .ok_or(ParseError("buffer is truncated"))?;
    Ok(u64::from_le_bytes(value.try_into().expect("eight bytes")))
}
