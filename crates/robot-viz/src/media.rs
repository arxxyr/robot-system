//! 有界媒体解码。只接受显式支持的布局、端序与编码，不猜测物理单位。

use robot_msg::msg;

use crate::{MAX_BLOB_BYTES, MAX_ELEMENTS, VizError, invalid};

/// 已复制的图像像素；depth16 保留原始深度值，未声明米制比例。
#[derive(Debug, Clone, PartialEq)]
pub enum Pixels {
    Rgb(Vec<u8>),
    Mono(Vec<u8>),
    Depth(Vec<u16>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PointCloud {
    pub points: Vec<[f32; 3]>,
    pub intensity: Option<Vec<f32>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    pub shape: Vec<u64>,
    pub values: Vec<f64>,
}

fn descriptor<'a>(
    desc: Option<msg::BlobDesc<'a>>,
    bytes: &[u8],
) -> Result<msg::BlobDesc<'a>, VizError> {
    let desc = desc.ok_or_else(|| invalid("缺少 BlobDesc"))?;
    if bytes.len() > MAX_BLOB_BYTES || desc.size() != bytes.len() as u64 || desc.offset() != 0 {
        return Err(invalid("blob 大小/偏移不一致或超出 16 MiB"));
    }
    if desc.compression() != msg::Compression::None_ {
        return Err(invalid("暂不支持压缩 blob"));
    }
    if desc.endian() != msg::Endian::Little && desc.endian() != msg::Endian::Big {
        return Err(invalid("未知 blob 端序"));
    }
    Ok(desc)
}

fn dimensions(desc: msg::BlobDesc<'_>, limit: usize) -> Result<Vec<u64>, VizError> {
    let shape = desc.shape().ok_or_else(|| invalid("缺少 blob shape"))?;
    if shape.is_empty() || shape.len() > 8 {
        return Err(invalid("shape 维数必须为 1..8"));
    }
    let mut count = 1usize;
    let mut result = Vec::with_capacity(shape.len());
    for dim in shape {
        count = count
            .checked_mul(dim as usize)
            .ok_or_else(|| invalid("shape 溢出"))?;
        if dim == 0 || count > limit {
            return Err(invalid("shape 为空或超过元素上限"));
        }
        result.push(u64::from(dim));
    }
    Ok(result)
}

fn contiguous(desc: msg::BlobDesc<'_>, shape: &[u64], element_size: usize) -> Result<(), VizError> {
    if let Some(strides) = desc.strides()
        && !strides.is_empty()
    {
        if strides.len() != shape.len() {
            return Err(invalid("strides 维数不匹配"));
        }
        let mut expected = element_size as u64;
        for index in (0..shape.len()).rev() {
            if u64::try_from(strides.get(index)).ok() != Some(expected) {
                return Err(invalid("只支持连续正向 strides"));
            }
            expected = expected
                .checked_mul(shape[index])
                .ok_or_else(|| invalid("strides 溢出"))?;
        }
    }
    Ok(())
}

pub fn image(header: msg::ImageHeader<'_>, bytes: &[u8]) -> Result<Image, VizError> {
    let desc = descriptor(header.blob(), bytes)?;
    let shape = dimensions(desc, MAX_BLOB_BYTES)?;
    if desc.layout() != msg::BlobLayout::HWC {
        return Err(invalid("图像只支持 HWC"));
    }
    let encoding = header
        .encoding()
        .ok_or_else(|| invalid("图像缺少 encoding"))?;
    let (channels, element_size, dtype) = match encoding {
        "rgb8" | "bgr8" => (3usize, 1usize, msg::Dtype::U8),
        "mono8" => (1, 1, msg::Dtype::U8),
        "depth16" => (1, 2, msg::Dtype::U16),
        _ => return Err(invalid("不支持的图像编码")),
    };
    let expected = [
        u64::from(header.height()),
        u64::from(header.width()),
        channels as u64,
    ];
    if desc.dtype() != dtype || (shape != expected && !(channels == 1 && shape == expected[..2])) {
        return Err(invalid("图像 dtype/shape 与编码及宽高不一致"));
    }
    let row = (header.width() as usize)
        .checked_mul(channels * element_size)
        .ok_or_else(|| invalid("图像行溢出"))?;
    let step = header.step() as usize;
    if step < row || step.checked_mul(header.height() as usize) != Some(bytes.len()) {
        return Err(invalid("图像 step 或 blob 字节数不一致"));
    }
    // 允许按 step 填充行；显式 strides 必须与同一 HWC 布局一致。
    if let Some(strides) = desc.strides() {
        let expected_strides: Vec<usize> = match shape.len() {
            2 => vec![step, element_size],
            _ => vec![step, channels * element_size, element_size],
        };
        if !strides.is_empty()
            && (strides.len() != expected_strides.len()
                || strides
                    .iter()
                    .zip(&expected_strides)
                    .any(|(value, expected)| usize::try_from(value).ok() != Some(*expected)))
        {
            return Err(invalid("图像 strides 与 step 不一致"));
        }
    }
    let color = header.color_order().unwrap_or("");
    if (encoding == "rgb8" && !matches!(color, "" | "RGB"))
        || (encoding == "bgr8" && !matches!(color, "" | "BGR"))
    {
        return Err(invalid("图像 color_order 与 encoding 冲突"));
    }
    let mut packed = Vec::with_capacity(row * header.height() as usize);
    for source in bytes.chunks_exact(step) {
        packed.extend_from_slice(&source[..row]);
    }
    let pixels = match encoding {
        "rgb8" => Pixels::Rgb(packed),
        "bgr8" => {
            for pixel in packed.as_chunks_mut::<3>().0 {
                pixel.swap(0, 2);
            }
            Pixels::Rgb(packed)
        }
        "mono8" => Pixels::Mono(packed),
        "depth16" => Pixels::Depth(
            packed
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| decode_u16([pair[0], pair[1]], desc.endian()))
                .collect(),
        ),
        _ => unreachable!("编码已验证"),
    };
    Ok(Image {
        width: header.width(),
        height: header.height(),
        pixels,
    })
}

pub fn pointcloud(header: msg::PointCloudHeader<'_>, bytes: &[u8]) -> Result<PointCloud, VizError> {
    let desc = descriptor(header.blob(), bytes)?;
    let shape = dimensions(desc, MAX_ELEMENTS)?;
    let fields: Vec<_> = header
        .fields()
        .ok_or_else(|| invalid("点云缺少 fields"))?
        .iter()
        .collect();
    let stride = match (desc.layout(), fields.as_slice()) {
        (msg::BlobLayout::PointsXYZ, ["x", "y", "z"]) => 3usize,
        (msg::BlobLayout::PointsXYZI, ["x", "y", "z", "intensity"]) => 4,
        _ => return Err(invalid("点云仅支持显式 XYZ/XYZI 字段顺序")),
    };
    if desc.dtype() != msg::Dtype::F32
        || shape != [u64::from(header.point_count()), stride as u64]
        || header.point_step() as usize != stride * 4
        || bytes.len() != header.point_count() as usize * stride * 4
    {
        return Err(invalid("点云 dtype/shape/point_step/size 不一致"));
    }
    contiguous(desc, &shape, 4)?;
    let mut points = Vec::with_capacity(header.point_count() as usize);
    let mut intensity = (stride == 4).then(|| Vec::with_capacity(header.point_count() as usize));
    for point in bytes.chunks_exact(stride * 4) {
        let mut components = [0.0f32; 4];
        for (index, chunk) in point.as_chunks::<4>().0.iter().enumerate() {
            let word = [chunk[0], chunk[1], chunk[2], chunk[3]];
            components[index] = match desc.endian() {
                msg::Endian::Big => f32::from_be_bytes(word),
                _ => f32::from_le_bytes(word),
            };
            if !components[index].is_finite() {
                return Err(invalid("点云包含非有限值"));
            }
        }
        points.push([components[0], components[1], components[2]]);
        if let Some(values) = &mut intensity {
            values.push(components[3]);
        }
    }
    Ok(PointCloud { points, intensity })
}

pub fn tensor(header: msg::TensorHeader<'_>, bytes: &[u8]) -> Result<Tensor, VizError> {
    let desc = descriptor(header.blob(), bytes)?;
    if header.device() != msg::Device::Cpu
        || header.quantization().is_some_and(|value| !value.is_empty())
    {
        return Err(invalid("只支持未量化 CPU 张量"));
    }
    let shape = dimensions(desc, MAX_ELEMENTS)?;
    let size = match desc.dtype() {
        msg::Dtype::U8 | msg::Dtype::I8 => 1,
        msg::Dtype::U16 | msg::Dtype::I16 | msg::Dtype::F16 => 2,
        msg::Dtype::F32 | msg::Dtype::I32 | msg::Dtype::U32 => 4,
        msg::Dtype::F64 => 8,
        _ => return Err(invalid("未知张量 dtype")),
    };
    contiguous(desc, &shape, size)?;
    let count: u64 = shape.iter().product();
    if count * size as u64 != bytes.len() as u64 {
        return Err(invalid("张量 shape 与字节数不一致"));
    }
    let big = desc.endian() == msg::Endian::Big;
    let mut values =
        Vec::with_capacity(usize::try_from(count).map_err(|_| invalid("元素数超出平台范围"))?);
    for chunk in bytes.chunks_exact(size) {
        let value = match desc.dtype() {
            msg::Dtype::U8 => f64::from(chunk[0]),
            msg::Dtype::I8 => f64::from(i8::from_ne_bytes([chunk[0]])),
            msg::Dtype::U16 => f64::from(decode_u16([chunk[0], chunk[1]], desc.endian())),
            msg::Dtype::I16 => f64::from(if big {
                i16::from_be_bytes([chunk[0], chunk[1]])
            } else {
                i16::from_le_bytes([chunk[0], chunk[1]])
            }),
            msg::Dtype::F16 => half_to_f64(decode_u16([chunk[0], chunk[1]], desc.endian())),
            msg::Dtype::F32 => f64::from(if big {
                f32::from_be_bytes(chunk.try_into().map_err(|_| invalid("F32 长度"))?)
            } else {
                f32::from_le_bytes(chunk.try_into().map_err(|_| invalid("F32 长度"))?)
            }),
            msg::Dtype::I32 => f64::from(if big {
                i32::from_be_bytes(chunk.try_into().map_err(|_| invalid("I32 长度"))?)
            } else {
                i32::from_le_bytes(chunk.try_into().map_err(|_| invalid("I32 长度"))?)
            }),
            msg::Dtype::U32 => f64::from(if big {
                u32::from_be_bytes(chunk.try_into().map_err(|_| invalid("U32 长度"))?)
            } else {
                u32::from_le_bytes(chunk.try_into().map_err(|_| invalid("U32 长度"))?)
            }),
            msg::Dtype::F64 => {
                if big {
                    f64::from_be_bytes(chunk.try_into().map_err(|_| invalid("F64 长度"))?)
                } else {
                    f64::from_le_bytes(chunk.try_into().map_err(|_| invalid("F64 长度"))?)
                }
            }
            _ => return Err(invalid("未知张量 dtype")),
        };
        if !value.is_finite() {
            return Err(invalid("张量包含非有限值"));
        }
        values.push(value);
    }
    Ok(Tensor { shape, values })
}

fn decode_u16(bytes: [u8; 2], endian: msg::Endian) -> u16 {
    match endian {
        msg::Endian::Big => u16::from_be_bytes(bytes),
        _ => u16::from_le_bytes(bytes),
    }
}

fn half_to_f64(bits: u16) -> f64 {
    let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exponent = (bits >> 10) & 0x1f;
    let mantissa = bits & 0x3ff;
    match exponent {
        0 => sign * f64::from(mantissa) * 2.0f64.powi(-24),
        31 if mantissa == 0 => sign * f64::INFINITY,
        31 => f64::NAN,
        _ => sign * (1.0 + f64::from(mantissa) / 1024.0) * 2.0f64.powi(i32::from(exponent) - 15),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flatbuffers::FlatBufferBuilder;

    fn image_body(
        encoding: &str,
        width: u32,
        height: u32,
        step: u32,
        size: usize,
        endian: msg::Endian,
    ) -> Vec<u8> {
        let mut builder = FlatBufferBuilder::new();
        let channels = if matches!(encoding, "rgb8" | "bgr8") {
            3
        } else {
            1
        };
        let shape = builder.create_vector(&[height, width, channels]);
        let desc = msg::BlobDesc::create(
            &mut builder,
            &msg::BlobDescArgs {
                shape: Some(shape),
                size: u64::try_from(size).unwrap(),
                alignment: 1,
                dtype: if encoding == "depth16" {
                    msg::Dtype::U16
                } else {
                    msg::Dtype::U8
                },
                layout: msg::BlobLayout::HWC,
                endian,
                ..Default::default()
            },
        );
        let encoding = builder.create_string(encoding);
        let image = msg::ImageHeader::create(
            &mut builder,
            &msg::ImageHeaderArgs {
                blob: Some(desc),
                width,
                height,
                step,
                encoding: Some(encoding),
                ..Default::default()
            },
        );
        builder.finish(image, None);
        builder.finished_data().to_vec()
    }

    #[test]
    fn camera_1080p_and_row_padding_are_supported_without_tensor_budget() {
        let bytes = vec![128; 1920 * 1080 * 3];
        let body = image_body(
            "rgb8",
            1920,
            1080,
            1920 * 3,
            bytes.len(),
            msg::Endian::Little,
        );
        let decoded = image(
            flatbuffers::root::<msg::ImageHeader>(&body).unwrap(),
            &bytes,
        )
        .unwrap();
        assert_eq!((decoded.width, decoded.height), (1920, 1080));
        assert_eq!(decoded.pixels, Pixels::Rgb(bytes));
        let body = image_body("bgr8", 1, 2, 4, 8, msg::Endian::Little);
        let decoded = image(
            flatbuffers::root::<msg::ImageHeader>(&body).unwrap(),
            &[3, 2, 1, 99, 6, 5, 4, 99],
        )
        .unwrap();
        assert_eq!(decoded.pixels, Pixels::Rgb(vec![1, 2, 3, 4, 5, 6]));
    }

    #[test]
    fn mono_and_big_endian_depth_preserve_pixels() {
        let body = image_body("mono8", 2, 1, 2, 2, msg::Endian::Little);
        assert_eq!(
            image(
                flatbuffers::root::<msg::ImageHeader>(&body).unwrap(),
                &[4, 9]
            )
            .unwrap()
            .pixels,
            Pixels::Mono(vec![4, 9])
        );
        let body = image_body("depth16", 2, 1, 4, 4, msg::Endian::Big);
        assert_eq!(
            image(
                flatbuffers::root::<msg::ImageHeader>(&body).unwrap(),
                &[0x12, 0x34, 0, 0xff]
            )
            .unwrap()
            .pixels,
            Pixels::Depth(vec![0x1234, 255])
        );
    }

    #[test]
    fn image_rejects_unknown_encoding_wrong_stride_and_truncated_blob() {
        for (encoding, step, size) in [("yuv_unknown", 3, 3), ("rgb8", 2, 2), ("rgb8", 3, 4)] {
            let body = image_body(encoding, 1, 1, step, size, msg::Endian::Little);
            assert!(
                image(
                    flatbuffers::root::<msg::ImageHeader>(&body).unwrap(),
                    &vec![0; size]
                )
                .is_err()
            );
        }
    }

    fn tensor_body(
        dtype: msg::Dtype,
        device: msg::Device,
        shape: &[u32],
        size: usize,
        compressed: bool,
    ) -> Vec<u8> {
        let mut builder = FlatBufferBuilder::new();
        let shape = builder.create_vector(shape);
        let desc = msg::BlobDesc::create(
            &mut builder,
            &msg::BlobDescArgs {
                shape: Some(shape),
                size: u64::try_from(size).unwrap(),
                alignment: 1,
                dtype,
                layout: msg::BlobLayout::Opaque,
                endian: msg::Endian::Big,
                compression: if compressed {
                    msg::Compression::Zstd
                } else {
                    msg::Compression::None_
                },
                ..Default::default()
            },
        );
        let tensor = msg::TensorHeader::create(
            &mut builder,
            &msg::TensorHeaderArgs {
                blob: Some(desc),
                device,
                ..Default::default()
            },
        );
        builder.finish(tensor, None);
        builder.finished_data().to_vec()
    }

    #[test]
    fn cpu_tensor_preserves_u32_and_half_values_with_explicit_endian() {
        let body = tensor_body(msg::Dtype::U32, msg::Device::Cpu, &[1, 2], 8, false);
        let bytes: Vec<_> = [u32::MAX, 16_777_217]
            .into_iter()
            .flat_map(u32::to_be_bytes)
            .collect();
        assert_eq!(
            tensor(
                flatbuffers::root::<msg::TensorHeader>(&body).unwrap(),
                &bytes
            )
            .unwrap()
            .values,
            vec![4_294_967_295.0, 16_777_217.0]
        );
        let body = tensor_body(msg::Dtype::F16, msg::Device::Cpu, &[2], 4, false);
        assert_eq!(
            tensor(
                flatbuffers::root::<msg::TensorHeader>(&body).unwrap(),
                &[0x3c, 0, 0xc0, 0]
            )
            .unwrap()
            .values,
            vec![1.0, -2.0]
        );
    }

    #[test]
    fn tensor_rejects_gpu_compression_shape_bomb_and_nonfinite_values() {
        for (device, shape, compressed) in [
            (msg::Device::Cuda, vec![1], false),
            (msg::Device::Cpu, vec![1], true),
            (msg::Device::Cpu, vec![u32::MAX, u32::MAX], false),
        ] {
            let body = tensor_body(msg::Dtype::F32, device, &shape, 4, compressed);
            assert!(
                tensor(
                    flatbuffers::root::<msg::TensorHeader>(&body).unwrap(),
                    &[0; 4]
                )
                .is_err()
            );
        }
        let body = tensor_body(msg::Dtype::F32, msg::Device::Cpu, &[1], 4, false);
        assert!(
            tensor(
                flatbuffers::root::<msg::TensorHeader>(&body).unwrap(),
                &f32::NAN.to_be_bytes()
            )
            .is_err()
        );
    }
}
