struct Centroids {
    count: u32;
    data: array<f32>;
};

struct Indices {
    data: array<u32>;
};

struct AtomicBuffer {
    data: array<atomic<u32>>;
};

struct KIndex {
    k: u32;
};

struct Settings {
    n_seq: u32;
    convergence: f32;
};

[[group(0), binding(0)]] var<storage, read_write> centroids: Centroids;
[[group(0), binding(1)]] var<storage, read> calculated: Indices;
[[group(0), binding(2)]] var pixels: texture_2d<f32>;
[[group(1), binding(0)]] var<storage, read_write> prefix_buffer: AtomicBuffer;
[[group(1), binding(1)]] var<storage, read_write> flag_buffer: AtomicBuffer;
[[group(1), binding(2)]] var<storage, read_write> convergence: AtomicBuffer;
[[group(1), binding(3)]] var<uniform> settings: Settings;
[[group(2), binding(0)]] var<uniform> k_index: KIndex;

let workgroup_size: u32 = 256u;

var<workgroup> scratch: array<vec4<f32>, workgroup_size>;
var<workgroup> shared_prefix: vec4<f32>;
var<workgroup> shared_flag: u32;

let FLAG_NOT_READY = 0u;
let FLAG_AGGREGATE_READY = 1u;
let FLAG_PREFIX_READY = 2u;

fn coords(global_x: u32, dimensions: vec2<i32>) -> vec2<i32> {
    return vec2<i32>(vec2<u32>(global_x % u32(dimensions.x), global_x / u32(dimensions.x)));
}

fn last_group_idx() -> u32 {
    return arrayLength(&flag_buffer.data) - 1u;
}

fn in_bounds(global_x: u32, dimensions: vec2<i32>) -> bool {
    let x = global_x % u32(dimensions.x);
    let y = global_x / u32(dimensions.x);
    return x < u32(dimensions.x) && y < u32(dimensions.y);
}

fn match_centroid(k: u32, global_x: u32) -> bool {
    return calculated.data[global_x] == k;
}

fn atomicStorePrefixVec(index: u32, value: vec4<f32>) {
    atomicStore(&prefix_buffer.data[index + 0u], bitcast<u32>(value.r));
    atomicStore(&prefix_buffer.data[index + 1u], bitcast<u32>(value.g));
    atomicStore(&prefix_buffer.data[index + 2u], bitcast<u32>(value.b));
    atomicStore(&prefix_buffer.data[index + 3u], bitcast<u32>(value.a));
}

fn atomicLoadPrefixVec(index: u32) -> vec4<f32> {
    let r = bitcast<f32>(atomicLoad(&prefix_buffer.data[index + 0u]));
    let g = bitcast<f32>(atomicLoad(&prefix_buffer.data[index + 1u]));
    let b = bitcast<f32>(atomicLoad(&prefix_buffer.data[index + 2u]));
    let a = bitcast<f32>(atomicLoad(&prefix_buffer.data[index + 3u]));
    return vec4<f32>(r, g, b, a);
}

[[stage(compute), workgroup_size(256)]]
fn main(
    [[builtin(local_invocation_id)]] local_id : vec3<u32>,
    [[builtin(workgroup_id)]] workgroup_id : vec3<u32>,
    [[builtin(global_invocation_id)]] global_id : vec3<u32>,
) {
    if (atomicLoad(&convergence.data[centroids.count]) >= centroids.count) {
        return;
    }
    
    if (local_id.x == workgroup_size - 1u) {
        atomicStore(&flag_buffer.data[workgroup_id.x], FLAG_NOT_READY);
    }
    storageBarrier();

    let k = k_index.k;
    let N_SEQ = settings.n_seq;

    let dimensions = textureDimensions(pixels);
    let global_x = global_id.x;
   
    scratch[local_id.x] = vec4<f32>(0.0);

    var local: vec4<f32> = vec4<f32>(0.0);
    for (var i: u32 = 0u; i < N_SEQ; i = i + 1u) {
        if (in_bounds(global_x * N_SEQ + i, dimensions) && match_centroid(k, global_x * N_SEQ + i)) {
            local = local + vec4<f32>(textureLoad(pixels, coords(global_x * N_SEQ + i, dimensions), 0).rgb, 1.0);
        }
    }

    scratch[local_id.x] = local;
    workgroupBarrier();
    
    for (var i: u32 = 0u; i < 8u; i = i + 1u) {
        workgroupBarrier();
        if (local_id.x >= (1u << i)) {
            local = local + scratch[local_id.x - (1u << i)];
        }
        workgroupBarrier();
        scratch[local_id.x] = local;
    }
    
    var exclusive_prefix = vec4<f32>(0.0);
    var flag = FLAG_AGGREGATE_READY;
    
    if (local_id.x == workgroup_size - 1u) {
        atomicStorePrefixVec(workgroup_id.x * 8u + 4u, local);
        if (workgroup_id.x == 0u) {
            // Special case, group 0 will not need to sum prefix.
            atomicStorePrefixVec(workgroup_id.x * 8u + 0u, local);
            flag = FLAG_PREFIX_READY;
        }
    }

    storageBarrier();
    if (local_id.x == workgroup_size - 1u) {
        atomicStore(&flag_buffer.data[workgroup_id.x], flag);
    }

    if(workgroup_id.x != 0u) {
        // decoupled loop-back
        var loop_back_ix = workgroup_id.x - 1u;
        loop {
            if(local_id.x == workgroup_size - 1u) {
                shared_flag = atomicLoad(&flag_buffer.data[loop_back_ix]);
            }
            workgroupBarrier();
            flag = shared_flag;
            storageBarrier();

            if (flag == FLAG_PREFIX_READY) {
                if (local_id.x == workgroup_size - 1u) {
                    let their_prefix = atomicLoadPrefixVec(loop_back_ix * 8u);
                    exclusive_prefix = exclusive_prefix + their_prefix;
                }
                break;
            } else if (flag == FLAG_AGGREGATE_READY) {                
                if (local_id.x == workgroup_size - 1u) {                    
                    let their_aggregate = atomicLoadPrefixVec(loop_back_ix * 8u + 4u);
                    exclusive_prefix = their_aggregate + exclusive_prefix;
                }
                loop_back_ix = loop_back_ix - 1u;
            }
            // else spin
        }

        // compute inclusive prefix
        storageBarrier();
        if (local_id.x == workgroup_size - 1u) {
            let inclusive_prefix = exclusive_prefix + local;
            shared_prefix = exclusive_prefix;
            
            atomicStorePrefixVec(workgroup_id.x * 8u + 0u, inclusive_prefix);
        }
        storageBarrier();
        if (local_id.x == workgroup_size - 1u) {
            atomicStore(&flag_buffer.data[workgroup_id.x], FLAG_PREFIX_READY);
        }
    }

    var prefix = vec4<f32>(0.0);
    workgroupBarrier();
    if(workgroup_id.x != 0u){
        prefix = shared_prefix;
    }

    if (workgroup_id.x == last_group_idx() & local_id.x == workgroup_size - 1u) {
        let sum = prefix + scratch[local_id.x];
        if(sum.a > 0.0) {
            let new_centroid = vec4<f32>(sum.rgb / sum.a, 1.0);
            let previous_centroid = vec4<f32>(
                centroids.data[k * 4u + 0u],
                centroids.data[k * 4u + 1u],
                centroids.data[k * 4u + 2u],
                centroids.data[k * 4u + 3u],
            );

            centroids.data[k * 4u + 0u] = new_centroid.r;
            centroids.data[k * 4u + 1u] = new_centroid.g;
            centroids.data[k * 4u + 2u] = new_centroid.b;
            centroids.data[k * 4u + 3u] = new_centroid.a;

            atomicStore(&convergence.data[k], u32(distance(new_centroid, previous_centroid) < settings.convergence));
        }

        if (k == centroids.count - 1u) {
            var converge = atomicLoad(&convergence.data[0u]);
            for (var i = 1u; i < centroids.count; i = i + 1u) {
                converge = converge + atomicLoad(&convergence.data[i]);
            }
            atomicStore(&convergence.data[centroids.count], converge);
        }
    }
    storageBarrier();
}
