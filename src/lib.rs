use anyhow::Result;
use palette::{FromColor, IntoColor, Lab, Pixel, Srgb, Srgba};
use pollster::FutureExt;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::{
    num::{NonZeroU32, NonZeroU64},
    vec,
};
use wgpu::{
    util::{BufferInitDescriptor, DeviceExt},
    BindGroupDescriptor, BindGroupEntry, BindGroupLayoutEntry, BindingResource, Buffer,
    BufferAddress, BufferBinding, BufferDescriptor, BufferUsages, Features, ShaderStages,
    TextureFormat, TextureViewDescriptor, TextureViewDimension,
};

const WORKGROUP_SIZE: u32 = 256;
const N_SEQ: u32 = 24;

pub struct Image {
    pub(crate) dimensions: (u32, u32),
    pub(crate) rgba: Vec<u8>,
}

impl Image {
    pub fn new(dimensions: (u32, u32), rbga: Vec<u8>) -> Self {
        Self {
            dimensions,
            rgba: rbga,
        }
    }

    pub fn get_pixel(&self, x: u32, y: u32) -> &[u8] {
        let index = (x + y * self.dimensions.0) as usize * 4;
        &self.rgba[index..index + 4]
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.dimensions
    }

    pub fn raw_pixels(&self) -> &[u8] {
        &self.rgba
    }
}

pub fn kmeans(k: u32, image: &Image) -> Result<Image> {
    let (width, height) = image.dimensions;

    let centroids = init_centroids(image, k);

    let instance = wgpu::Instance::new(wgpu::Backends::all());
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptionsBase {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        })
        .block_on()
        .ok_or_else(|| anyhow::anyhow!("Couldn't create the adapter"))?;

    let features = adapter.features();
    let (device, queue) = adapter
        .request_device(
            &wgpu::DeviceDescriptor {
                label: None,
                features: features & (Features::TIMESTAMP_QUERY),
                limits: Default::default(),
            },
            None,
        )
        .block_on()?;

    let query_set = if features.contains(Features::TIMESTAMP_QUERY) {
        Some(device.create_query_set(&wgpu::QuerySetDescriptor {
            count: 2,
            ty: wgpu::QueryType::Timestamp,
            label: None,
        }))
    } else {
        None
    };
    let query_buf = device.create_buffer_init(&BufferInitDescriptor {
        label: None,
        contents: &[0; 16],
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
    });

    let texture_size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };

    let input_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("input texture"),
        size: texture_size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
    });
    let float_data = Srgba::from_raw_slice(&image.rgba)
        .into_iter()
        .map(|color| color.into_format::<f32, f32>().into_raw::<[f32; 4]>())
        .flatten()
        .collect::<Vec<_>>();

    queue.write_texture(
        input_texture.as_image_copy(),
        bytemuck::cast_slice(&float_data),
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: std::num::NonZeroU32::new(16 * width),
            rows_per_image: None,
        },
        texture_size,
    );

    // let output_texture = device.create_texture(&wgpu::TextureDescriptor {
    //     label: Some("output texture"),
    //     size: texture_size,
    //     mip_level_count: 1,
    //     sample_count: 1,
    //     dimension: wgpu::TextureDimension::D2,
    //     format: wgpu::TextureFormat::Rgba8Unorm,
    //     usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::STORAGE_BINDING,
    // });

    let centroid_buffer = device.create_buffer_init(&BufferInitDescriptor {
        label: None,
        contents: &centroids,
        usage: BufferUsages::STORAGE | BufferUsages::COPY_SRC | BufferUsages::COPY_DST,
    });

    let index_size = width * height;
    let calculated_buffer = device.create_buffer_init(&BufferInitDescriptor {
        label: None,
        contents: bytemuck::cast_slice::<u32, u8>(&vec![k + 1; index_size as usize]),
        usage: BufferUsages::STORAGE | BufferUsages::COPY_SRC,
    });

    let find_centroid_shader = device.create_shader_module(&wgpu::ShaderModuleDescriptor {
        label: Some("Find centroid shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/find_centroid.wgsl").into()),
    });

    let find_centroid_bind_group_layout =
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Find centroid bind group layout"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

    let find_centroid_pipeline_layout =
        device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline layout"),
            bind_group_layouts: &[&find_centroid_bind_group_layout],
            push_constant_ranges: &[],
        });

    let find_centroid_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("Find centroid pipeline"),
        layout: Some(&find_centroid_pipeline_layout),
        module: &find_centroid_shader,
        entry_point: "main",
    });

    let find_centroid_bind_group = device.create_bind_group(&BindGroupDescriptor {
        label: Some("Find centroid bind group"),
        layout: &find_centroid_bind_group_layout,
        entries: &[
            BindGroupEntry {
                binding: 0,
                resource: BindingResource::TextureView(&input_texture.create_view(
                    &TextureViewDescriptor {
                        label: None,
                        format: Some(TextureFormat::Rgba32Float),
                        aspect: wgpu::TextureAspect::All,
                        base_mip_level: 0,
                        mip_level_count: NonZeroU32::new(1),
                        dimension: Some(TextureViewDimension::D2),
                        ..Default::default()
                    },
                )),
            },
            BindGroupEntry {
                binding: 1,
                resource: centroid_buffer.as_entire_binding(),
            },
            BindGroupEntry {
                binding: 2,
                resource: calculated_buffer.as_entire_binding(),
            },
        ],
    });

    // let choose_centroid_shader = device.create_shader_module(&wgpu::ShaderModuleDescriptor {
    //     label: Some("Find centroid shader"),
    //     source: wgpu::ShaderSource::Wgsl(include_str!("shaders/choose_centroid.wgsl").into()),
    // });

    // let choose_centroid_pipeline =
    //     device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
    //         label: Some("Find centroid pipeline"),
    //         layout: None,
    //         module: &choose_centroid_shader,
    //         entry_point: "main",
    //     });

    // let choose_centroid_bind_group_0 = device.create_bind_group(&BindGroupDescriptor {
    //     label: None,
    //     layout: &choose_centroid_pipeline.get_bind_group_layout(0),
    //     entries: &[
    //         BindGroupEntry {
    //             binding: 0,
    //             resource: centroid_buffer.as_entire_binding(),
    //         },
    //         BindGroupEntry {
    //             binding: 1,
    //             resource: calculated_buffer.as_entire_binding(),
    //         },
    //         BindGroupEntry {
    //             binding: 2,
    //             resource: BindingResource::TextureView(
    //                 &input_texture.create_view(&TextureViewDescriptor::default()),
    //             ),
    //         },
    //     ],
    // });

    // let choose_centroid_settings_buffer = device.create_buffer_init(&BufferInitDescriptor {
    //     label: None,
    //     contents: bytemuck::cast_slice(&[N_SEQ]),
    //     usage: BufferUsages::UNIFORM,
    // });

    // let (choose_centroid_dispatch_width, _) = compute_work_group_count(
    //     (texture_size.width * texture_size.height, 1),
    //     (WORKGROUP_SIZE * N_SEQ, 1),
    // );
    // let color_buffer_size = choose_centroid_dispatch_width * 8 * 4;
    // let color_buffer = device.create_buffer(&BufferDescriptor {
    //     label: None,
    //     size: color_buffer_size as BufferAddress,
    //     usage: BufferUsages::STORAGE,
    //     mapped_at_creation: false,
    // });
    // let state_buffer_size = choose_centroid_dispatch_width;
    // let state_buffer = device.create_buffer_init(&BufferInitDescriptor {
    //     label: None,
    //     contents: bytemuck::cast_slice::<u32, u8>(&vec![0; state_buffer_size as usize]),
    //     usage: BufferUsages::STORAGE,
    // });
    // let convergence_buffer = device.create_buffer_init(&BufferInitDescriptor {
    //     label: None,
    //     contents: bytemuck::cast_slice::<u32, u8>(&vec![0; k as usize + 1]),
    //     usage: BufferUsages::STORAGE,
    // });

    // let choose_centroid_bind_group_1 = device.create_bind_group(&BindGroupDescriptor {
    //     label: None,
    //     layout: &choose_centroid_pipeline.get_bind_group_layout(1),
    //     entries: &[
    //         BindGroupEntry {
    //             binding: 0,
    //             resource: color_buffer.as_entire_binding(),
    //         },
    //         BindGroupEntry {
    //             binding: 1,
    //             resource: state_buffer.as_entire_binding(),
    //         },
    //         BindGroupEntry {
    //             binding: 2,
    //             resource: convergence_buffer.as_entire_binding(),
    //         },
    //         BindGroupEntry {
    //             binding: 3,
    //             resource: choose_centroid_settings_buffer.as_entire_binding(),
    //         },
    //     ],
    // });

    // let k_index_buffers: Vec<Buffer> = (0..k)
    //     .map(|k| {
    //         device.create_buffer_init(&BufferInitDescriptor {
    //             label: None,
    //             contents: bytemuck::cast_slice(&[k]),
    //             usage: BufferUsages::UNIFORM,
    //         })
    //     })
    //     .collect();

    // let k_index_bind_groups: Vec<_> = (0..k)
    //     .map(|k| {
    //         device.create_bind_group(&BindGroupDescriptor {
    //             label: None,
    //             layout: &choose_centroid_pipeline.get_bind_group_layout(2),
    //             entries: &[BindGroupEntry {
    //                 binding: 0,
    //                 resource: BindingResource::Buffer(BufferBinding {
    //                     buffer: &k_index_buffers[k as usize],
    //                     offset: 0,
    //                     size: None,
    //                 }),
    //             }],
    //         })
    //     })
    //     .collect();

    let swap_shader = device.create_shader_module(&wgpu::ShaderModuleDescriptor {
        label: Some("Swap colors shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/swap.wgsl").into()),
    });

    // let swap_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
    //     label: Some("Swap pipeline"),
    //     layout: None,
    //     module: &swap_shader,
    //     entry_point: "main",
    // });

    // let swap_bind_group = device.create_bind_group(&BindGroupDescriptor {
    //     label: None,
    //     layout: &swap_pipeline.get_bind_group_layout(0),
    //     entries: &[
    //         BindGroupEntry {
    //             binding: 0,
    //             resource: centroid_buffer.as_entire_binding(),
    //         },
    //         BindGroupEntry {
    //             binding: 1,
    //             resource: calculated_buffer.as_entire_binding(),
    //         },
    //         BindGroupEntry {
    //             binding: 2,
    //             resource: BindingResource::TextureView(
    //                 &output_texture.create_view(&TextureViewDescriptor::default()),
    //             ),
    //         },
    //     ],
    // });

    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    if let Some(query_set) = &query_set {
        encoder.write_timestamp(query_set, 0);
    }

    let (dispatch_with, dispatch_height) =
        compute_work_group_count((texture_size.width, texture_size.height), (16, 16));
    {
        let mut compute_pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("Kmean pass"),
        });
        compute_pass.set_pipeline(&find_centroid_pipeline);
        compute_pass.set_bind_group(0, &find_centroid_bind_group, &[]);
        compute_pass.dispatch(dispatch_with, dispatch_height, 1);

        // for _ in 0..30 {
        //     compute_pass.set_pipeline(&choose_centroid_pipeline);
        //     compute_pass.set_bind_group(0, &choose_centroid_bind_group_0, &[]);
        //     compute_pass.set_bind_group(1, &choose_centroid_bind_group_1, &[]);
        //     for i in 0..k {
        //         compute_pass.set_bind_group(2, &k_index_bind_groups[i as usize], &[]);
        //         compute_pass.dispatch(choose_centroid_dispatch_width, 1, 1);
        //     }

        //     compute_pass.set_pipeline(&find_centroid_pipeline);
        //     compute_pass.set_bind_group(0, &find_centroid_bind_group, &[]);
        //     compute_pass.dispatch(dispatch_with, dispatch_height, 1);
        // }

        // compute_pass.set_pipeline(&swap_pipeline);
        // compute_pass.set_bind_group(0, &swap_bind_group, &[]);
        // compute_pass.dispatch(dispatch_with, dispatch_height, 1);
    }
    if let Some(query_set) = &query_set {
        encoder.write_timestamp(query_set, 1);
    }

    let padded_bytes_per_row = padded_bytes_per_row(width);
    let unpadded_bytes_per_row = width as usize * 4;

    let output_buffer_size =
        padded_bytes_per_row as u64 * height as u64 * std::mem::size_of::<u8>() as u64;
    let output_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: output_buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let centroid_size = centroids.len() as BufferAddress;
    let staging_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: centroid_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    // encoder.copy_texture_to_buffer(
    //     wgpu::ImageCopyTexture {
    //         aspect: wgpu::TextureAspect::All,
    //         texture: &output_texture,
    //         mip_level: 0,
    //         origin: wgpu::Origin3d::ZERO,
    //     },
    //     wgpu::ImageCopyBuffer {
    //         buffer: &output_buffer,
    //         layout: wgpu::ImageDataLayout {
    //             offset: 0,
    //             bytes_per_row: std::num::NonZeroU32::new(padded_bytes_per_row as u32),
    //             rows_per_image: std::num::NonZeroU32::new(height),
    //         },
    //     },
    //     texture_size,
    // );

    encoder.copy_buffer_to_buffer(&centroid_buffer, 0, &staging_buffer, 0, centroid_size);

    if let Some(query_set) = &query_set {
        encoder.resolve_query_set(query_set, 0..2, &query_buf, 0);
    }
    queue.submit(Some(encoder.finish()));

    let buffer_slice = output_buffer.slice(..);
    let buffer_future = buffer_slice.map_async(wgpu::MapMode::Read);

    let cent_buffer_slice = staging_buffer.slice(..);
    let cent_buffer_future = cent_buffer_slice.map_async(wgpu::MapMode::Read);

    let query_slice = query_buf.slice(..);
    let query_future = query_slice.map_async(wgpu::MapMode::Read);

    device.poll(wgpu::Maintain::Wait);

    if let Ok(()) = cent_buffer_future.block_on() {
        let data = cent_buffer_slice.get_mapped_range();

        for (index, k) in bytemuck::cast_slice::<u8, f32>(&data[4..])
            .chunks(4)
            .enumerate()
        {
            println!("Centroid {index} = {k:?}")
        }
    }

    if query_future.block_on().is_ok() && features.contains(Features::TIMESTAMP_QUERY) {
        let ts_period = queue.get_timestamp_period();
        let ts_data_raw = &*query_slice.get_mapped_range();
        let ts_data: &[u64] = bytemuck::cast_slice(ts_data_raw);
        println!(
            "Compute shader elapsed: {:?}ms",
            (ts_data[1] - ts_data[0]) as f64 * ts_period as f64 * 1e-6
        );
    }

    match buffer_future.block_on() {
        Ok(()) => {
            let padded_data = buffer_slice.get_mapped_range();

            let mut pixels: Vec<u8> = vec![0; unpadded_bytes_per_row * height as usize];
            for (padded, pixels) in padded_data
                .chunks_exact(padded_bytes_per_row)
                .zip(pixels.chunks_exact_mut(unpadded_bytes_per_row))
            {
                pixels.copy_from_slice(&padded[..unpadded_bytes_per_row]);
            }

            let result = Image::new((width, height), pixels);

            Ok(result)
        }
        Err(e) => Err(e.into()),
    }
}

fn init_centroids(image: &Image, k: u32) -> Vec<u8> {
    let mut centroids: Vec<u8> = vec![];
    centroids.extend_from_slice(bytemuck::cast_slice(&[k]));

    let mut rng = StdRng::seed_from_u64(42);

    let (width, height) = image.dimensions;
    let total_px = width * height;
    let mut picked_indices = Vec::with_capacity(k as usize);

    for _ in 0..k {
        loop {
            let color_index = rng.gen_range(0..total_px);
            if !picked_indices.contains(&color_index) {
                picked_indices.push(color_index);
                break;
            }
        }
    }

    centroids.extend_from_slice(bytemuck::cast_slice(
        &picked_indices
            .into_iter()
            .flat_map(|color_index| {
                let x = color_index % width;
                let y = color_index / width;
                let pixel = image.get_pixel(x, y);
                [
                    pixel[0] as f32 / 255.0,
                    pixel[1] as f32 / 255.0,
                    pixel[2] as f32 / 255.0,
                    pixel[3] as f32 / 255.0,
                ]
            })
            .collect::<Vec<f32>>(),
    ));

    centroids
}

fn compute_work_group_count(
    (width, height): (u32, u32),
    (workgroup_width, workgroup_height): (u32, u32),
) -> (u32, u32) {
    let x = (width + workgroup_width - 1) / workgroup_width;
    let y = (height + workgroup_height - 1) / workgroup_height;

    (x, y)
}

/// Compute the next multiple of 256 for texture retrieval padding.
fn padded_bytes_per_row(width: u32) -> usize {
    let bytes_per_row = width as usize * 4;
    let padding = (256 - bytes_per_row % 256) % 256;
    bytes_per_row + padding
}
