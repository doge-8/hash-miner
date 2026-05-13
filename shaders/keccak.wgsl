// keccak256(challenge ‖ nonce) brute-force search.
// Shader logic adapted from hash256.org's WebGPU miner (same algorithm).

// 每个 GPU 线程串行试 ITERATIONS 个 nonce, 减少 dispatch 开销.
const ITERATIONS: u32 = 16u;

var<private> RC: array<vec2<u32>, 24> = array<vec2<u32>, 24>(
  vec2<u32>(0x00000001u, 0x00000000u),
  vec2<u32>(0x00008082u, 0x00000000u),
  vec2<u32>(0x0000808au, 0x80000000u),
  vec2<u32>(0x80008000u, 0x80000000u),
  vec2<u32>(0x0000808bu, 0x00000000u),
  vec2<u32>(0x80000001u, 0x00000000u),
  vec2<u32>(0x80008081u, 0x80000000u),
  vec2<u32>(0x00008009u, 0x80000000u),
  vec2<u32>(0x0000008au, 0x00000000u),
  vec2<u32>(0x00000088u, 0x00000000u),
  vec2<u32>(0x80008009u, 0x00000000u),
  vec2<u32>(0x8000000au, 0x00000000u),
  vec2<u32>(0x8000808bu, 0x00000000u),
  vec2<u32>(0x0000008bu, 0x80000000u),
  vec2<u32>(0x00008089u, 0x80000000u),
  vec2<u32>(0x00008003u, 0x80000000u),
  vec2<u32>(0x00008002u, 0x80000000u),
  vec2<u32>(0x00000080u, 0x80000000u),
  vec2<u32>(0x0000800au, 0x00000000u),
  vec2<u32>(0x8000000au, 0x80000000u),
  vec2<u32>(0x80008081u, 0x80000000u),
  vec2<u32>(0x00008080u, 0x80000000u),
  vec2<u32>(0x80000001u, 0x00000000u),
  vec2<u32>(0x80008008u, 0x80000000u),
);

fn rotl64(v: vec2<u32>, n: u32) -> vec2<u32> {
  let nn = n & 63u;
  if (nn == 0u)  { return v; }
  if (nn == 32u) { return vec2<u32>(v.y, v.x); }
  if (nn < 32u) {
    let m = 32u - nn;
    return vec2<u32>(
      (v.x << nn) | (v.y >> m),
      (v.y << nn) | (v.x >> m),
    );
  }
  let s = nn - 32u;
  let m = 32u - s;
  return vec2<u32>(
    (v.y << s) | (v.x >> m),
    (v.x << s) | (v.y >> m),
  );
}

fn xor64(a: vec2<u32>, b: vec2<u32>) -> vec2<u32> {
  return vec2<u32>(a.x ^ b.x, a.y ^ b.y);
}

fn andnot64(a: vec2<u32>, b: vec2<u32>) -> vec2<u32> {
  return vec2<u32>((~a.x) & b.x, (~a.y) & b.y);
}

fn bswap32(v: u32) -> u32 {
  return ((v & 0x000000ffu) << 24u)
       | ((v & 0x0000ff00u) <<  8u)
       | ((v & 0x00ff0000u) >>  8u)
       | ((v & 0xff000000u) >> 24u);
}

fn keccak_f1600(s: ptr<function, array<vec2<u32>, 25>>) {
  for (var r: u32 = 0u; r < 24u; r = r + 1u) {
    let C0 = xor64(xor64(xor64(xor64((*s)[0],  (*s)[5]),  (*s)[10]), (*s)[15]), (*s)[20]);
    let C1 = xor64(xor64(xor64(xor64((*s)[1],  (*s)[6]),  (*s)[11]), (*s)[16]), (*s)[21]);
    let C2 = xor64(xor64(xor64(xor64((*s)[2],  (*s)[7]),  (*s)[12]), (*s)[17]), (*s)[22]);
    let C3 = xor64(xor64(xor64(xor64((*s)[3],  (*s)[8]),  (*s)[13]), (*s)[18]), (*s)[23]);
    let C4 = xor64(xor64(xor64(xor64((*s)[4],  (*s)[9]),  (*s)[14]), (*s)[19]), (*s)[24]);

    let D0 = xor64(C4, rotl64(C1, 1u));
    let D1 = xor64(C0, rotl64(C2, 1u));
    let D2 = xor64(C1, rotl64(C3, 1u));
    let D3 = xor64(C2, rotl64(C4, 1u));
    let D4 = xor64(C3, rotl64(C0, 1u));

    let b00 = xor64((*s)[ 0], D0);
    let b10 = rotl64(xor64((*s)[ 1], D1),  1u);
    let b20 = rotl64(xor64((*s)[ 2], D2), 62u);
    let b05 = rotl64(xor64((*s)[ 3], D3), 28u);
    let b15 = rotl64(xor64((*s)[ 4], D4), 27u);
    let b16 = rotl64(xor64((*s)[ 5], D0), 36u);
    let b01 = rotl64(xor64((*s)[ 6], D1), 44u);
    let b11 = rotl64(xor64((*s)[ 7], D2),  6u);
    let b21 = rotl64(xor64((*s)[ 8], D3), 55u);
    let b06 = rotl64(xor64((*s)[ 9], D4), 20u);
    let b07 = rotl64(xor64((*s)[10], D0),  3u);
    let b17 = rotl64(xor64((*s)[11], D1), 10u);
    let b02 = rotl64(xor64((*s)[12], D2), 43u);
    let b12 = rotl64(xor64((*s)[13], D3), 25u);
    let b22 = rotl64(xor64((*s)[14], D4), 39u);
    let b23 = rotl64(xor64((*s)[15], D0), 41u);
    let b08 = rotl64(xor64((*s)[16], D1), 45u);
    let b18 = rotl64(xor64((*s)[17], D2), 15u);
    let b03 = rotl64(xor64((*s)[18], D3), 21u);
    let b13 = rotl64(xor64((*s)[19], D4),  8u);
    let b14 = rotl64(xor64((*s)[20], D0), 18u);
    let b24 = rotl64(xor64((*s)[21], D1),  2u);
    let b09 = rotl64(xor64((*s)[22], D2), 61u);
    let b19 = rotl64(xor64((*s)[23], D3), 56u);
    let b04 = rotl64(xor64((*s)[24], D4), 14u);

    (*s)[ 0] = xor64(b00, andnot64(b01, b02));
    (*s)[ 1] = xor64(b01, andnot64(b02, b03));
    (*s)[ 2] = xor64(b02, andnot64(b03, b04));
    (*s)[ 3] = xor64(b03, andnot64(b04, b00));
    (*s)[ 4] = xor64(b04, andnot64(b00, b01));
    (*s)[ 5] = xor64(b05, andnot64(b06, b07));
    (*s)[ 6] = xor64(b06, andnot64(b07, b08));
    (*s)[ 7] = xor64(b07, andnot64(b08, b09));
    (*s)[ 8] = xor64(b08, andnot64(b09, b05));
    (*s)[ 9] = xor64(b09, andnot64(b05, b06));
    (*s)[10] = xor64(b10, andnot64(b11, b12));
    (*s)[11] = xor64(b11, andnot64(b12, b13));
    (*s)[12] = xor64(b12, andnot64(b13, b14));
    (*s)[13] = xor64(b13, andnot64(b14, b10));
    (*s)[14] = xor64(b14, andnot64(b10, b11));
    (*s)[15] = xor64(b15, andnot64(b16, b17));
    (*s)[16] = xor64(b16, andnot64(b17, b18));
    (*s)[17] = xor64(b17, andnot64(b18, b19));
    (*s)[18] = xor64(b18, andnot64(b19, b15));
    (*s)[19] = xor64(b19, andnot64(b15, b16));
    (*s)[20] = xor64(b20, andnot64(b21, b22));
    (*s)[21] = xor64(b21, andnot64(b22, b23));
    (*s)[22] = xor64(b22, andnot64(b23, b24));
    (*s)[23] = xor64(b23, andnot64(b24, b20));
    (*s)[24] = xor64(b24, andnot64(b20, b21));

    (*s)[0] = xor64((*s)[0], RC[r]);
  }
}

struct Uniforms {
  challenge: array<vec4<u32>, 2>,
  difficulty: array<vec4<u32>, 2>,
  nonce_base_lo: u32,
  nonce_base_hi: u32,
  _pad0: u32,
  _pad1: u32,
};

struct ResultBuffer {
  found: atomic<u32>,
  nonce_lo: u32,
  nonce_hi: u32,
  _pad: u32,
  hash: array<vec4<u32>, 2>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var<storage, read_write> result: ResultBuffer;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
  let thread_start = gid.x * ITERATIONS;

  for (var k: u32 = 0u; k < ITERATIONS; k = k + 1u) {
    let offset = thread_start + k;

    let added = u.nonce_base_lo + offset;
    let carry = select(0u, 1u, added < u.nonce_base_lo);
    let n_lo  = added;
    let n_hi  = u.nonce_base_hi + carry;

    var st: array<vec2<u32>, 25>;

    // lanes 0..3 ← challenge bytes 0..31 (one lane per 8 bytes, LE).
    st[0] = vec2<u32>(u.challenge[0].x, u.challenge[0].y);
    st[1] = vec2<u32>(u.challenge[0].z, u.challenge[0].w);
    st[2] = vec2<u32>(u.challenge[1].x, u.challenge[1].y);
    st[3] = vec2<u32>(u.challenge[1].z, u.challenge[1].w);

    // lanes 4..6 ← zero (high 24 bytes of BE-encoded uint256 nonce).
    st[4] = vec2<u32>(0u, 0u);
    st[5] = vec2<u32>(0u, 0u);
    st[6] = vec2<u32>(0u, 0u);

    // lane 7 ← nonce as BE 8 bytes at input positions 56..63.
    st[7] = vec2<u32>(bswap32(n_hi), bswap32(n_lo));

    // lane 8 ← 0x01 at byte 64 (Keccak pad delimiter).
    st[8] = vec2<u32>(0x00000001u, 0x00000000u);

    st[ 9] = vec2<u32>(0u, 0u);
    st[10] = vec2<u32>(0u, 0u);
    st[11] = vec2<u32>(0u, 0u);
    st[12] = vec2<u32>(0u, 0u);
    st[13] = vec2<u32>(0u, 0u);
    st[14] = vec2<u32>(0u, 0u);
    st[15] = vec2<u32>(0u, 0u);

    // lane 16 ← 0x80 at byte 135 (final byte of padded block).
    st[16] = vec2<u32>(0u, 0x80000000u);

    st[17] = vec2<u32>(0u, 0u);
    st[18] = vec2<u32>(0u, 0u);
    st[19] = vec2<u32>(0u, 0u);
    st[20] = vec2<u32>(0u, 0u);
    st[21] = vec2<u32>(0u, 0u);
    st[22] = vec2<u32>(0u, 0u);
    st[23] = vec2<u32>(0u, 0u);
    st[24] = vec2<u32>(0u, 0u);

    keccak_f1600(&st);

    let h0 = bswap32(st[0].x);
    let h1 = bswap32(st[0].y);
    let h2 = bswap32(st[1].x);
    let h3 = bswap32(st[1].y);
    let h4 = bswap32(st[2].x);
    let h5 = bswap32(st[2].y);
    let h6 = bswap32(st[3].x);
    let h7 = bswap32(st[3].y);

    let d0 = u.difficulty[0].x;
    let d1 = u.difficulty[0].y;
    let d2 = u.difficulty[0].z;
    let d3 = u.difficulty[0].w;
    let d4 = u.difficulty[1].x;
    let d5 = u.difficulty[1].y;
    let d6 = u.difficulty[1].z;
    let d7 = u.difficulty[1].w;

    var lt = false;
    var settled = false;
    if (h0 < d0)      { lt = true;  settled = true; }
    else if (h0 > d0) {              settled = true; }
    if (!settled) { if (h1 < d1) { lt = true; settled = true; } else if (h1 > d1) { settled = true; } }
    if (!settled) { if (h2 < d2) { lt = true; settled = true; } else if (h2 > d2) { settled = true; } }
    if (!settled) { if (h3 < d3) { lt = true; settled = true; } else if (h3 > d3) { settled = true; } }
    if (!settled) { if (h4 < d4) { lt = true; settled = true; } else if (h4 > d4) { settled = true; } }
    if (!settled) { if (h5 < d5) { lt = true; settled = true; } else if (h5 > d5) { settled = true; } }
    if (!settled) { if (h6 < d6) { lt = true; settled = true; } else if (h6 > d6) { settled = true; } }
    if (!settled) { if (h7 < d7) { lt = true; } }

    if (lt) {
      let prior = atomicAdd(&result.found, 1u);
      if (prior == 0u) {
        result.nonce_lo = n_lo;
        result.nonce_hi = n_hi;
        result.hash[0]  = vec4<u32>(h0, h1, h2, h3);
        result.hash[1]  = vec4<u32>(h4, h5, h6, h7);
      }
      break;
    }
  }
}
