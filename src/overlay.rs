//! Composites the display plane textures (the title's UI overlay) onto the swap chain before it is presented.
//!
//! The newer SDK's titles get two display planes from the embedded context function `+0x6a0` and draw their UI into the second
//! plane's texture; on a console the display hardware blends the planes. The layer presents only the swap chain, so without this
//! the UI (Forza Horizon 3: the press-start screen) never appears. The blend is done with a private deferred context (its state is
//! separate from the title's) that is recorded and executed on the immediate context once per present.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryA(name: *const u8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

const HLSL: &str = r#"
struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
VSOut vs_main(uint id : SV_VertexID) {
    VSOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    o.uv = uv;
    return o;
}
Texture2D tex : register(t0);
SamplerState samp : register(s0);
float4 ps_main(VSOut i) : SV_Target {
    return tex.Sample(samp, i.uv);
}
"#;

struct State {
    ctx: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    blend: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    deferred: ID3D11DeviceContext,
    rtvs: HashMap<usize, ID3D11RenderTargetView>,
    srvs: HashMap<usize, ID3D11ShaderResourceView>,
}
unsafe impl Send for State {}

static STATE: Mutex<Option<State>> = Mutex::new(None);
static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

type D3DCompileFn = unsafe extern "system" fn(*const c_void, usize, *const u8, *const c_void, *const c_void, *const u8, *const u8, u32, u32, *mut *mut c_void, *mut *mut c_void) -> i32;

/// Compiles `source` (HLSL) for `target` (e.g. `b"cs_5_0\0"`) and `entry` (NUL-terminated) with d3dcompiler_47.
pub unsafe fn compile_hlsl(source: &str, target: &[u8], entry: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let m = LoadLibraryA(b"d3dcompiler_47.dll\0".as_ptr());
        if m.is_null() {
            return None;
        }
        let f = GetProcAddress(m, b"D3DCompile\0".as_ptr());
        if f.is_null() {
            return None;
        }
        let f: D3DCompileFn = std::mem::transmute(f);
        let mut code: *mut c_void = std::ptr::null_mut();
        let mut err: *mut c_void = std::ptr::null_mut();
        let hr = f(source.as_ptr() as *const c_void, source.len(), std::ptr::null(), std::ptr::null(), std::ptr::null(), entry.as_ptr(), target.as_ptr(), 0, 0, &mut code, &mut err);
        if hr < 0 || code.is_null() {
            eprintln!("[overlay] D3DCompile failed: {hr:#x}");
            return None;
        }
        let blob: ID3DBlob = Interface::from_raw(code);
        Some(std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()).to_vec())
    }
}

unsafe fn init(real_ctx: *mut c_void) -> Option<State> {
    unsafe {
        let ctx: ID3D11DeviceContext = ID3D11DeviceContext::from_raw_borrowed(&real_ctx)?.clone();
        let device = ctx.GetDevice().ok()?;
        let vs_code = compile_hlsl(HLSL, b"vs_5_0\0", b"vs_main\0")?;
        let ps_code = compile_hlsl(HLSL, b"ps_5_0\0", b"ps_main\0")?;
        let mut vs = None;
        device.CreateVertexShader(&vs_code, None, Some(&mut vs)).ok()?;
        let mut ps = None;
        device.CreatePixelShader(&ps_code, None, Some(&mut ps)).ok()?;
        let mut rt = D3D11_RENDER_TARGET_BLEND_DESC::default();
        rt.BlendEnable = true.into();
        rt.SrcBlend = D3D11_BLEND_ONE;
        rt.DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
        rt.BlendOp = D3D11_BLEND_OP_ADD;
        rt.SrcBlendAlpha = D3D11_BLEND_ONE;
        rt.DestBlendAlpha = D3D11_BLEND_INV_SRC_ALPHA;
        rt.BlendOpAlpha = D3D11_BLEND_OP_ADD;
        rt.RenderTargetWriteMask = 0xf;
        let mut bd = D3D11_BLEND_DESC::default();
        bd.RenderTarget[0] = rt;
        let mut blend = None;
        device.CreateBlendState(&bd, Some(&mut blend)).ok()?;
        let sd = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        device.CreateSamplerState(&sd, Some(&mut sampler)).ok()?;
        let mut deferred = None;
        device.CreateDeferredContext(0, Some(&mut deferred)).ok()?;
        eprintln!("[overlay] ready");
        Some(State { ctx, vs: vs?, ps: ps?, blend: blend?, sampler: sampler?, deferred: deferred?, rtvs: HashMap::new(), srvs: HashMap::new() })
    }
}

/// Blends each plane texture (premultiplied alpha) over `target` (the swap chain's buffer 0), on the immediate context `real_ctx`.
pub unsafe fn composite(real_ctx: *mut c_void, target: *mut c_void, planes: &[*mut c_void]) {
    if FAILED.load(std::sync::atomic::Ordering::Relaxed) || planes.is_empty() || target.is_null() {
        return;
    }
    let mut guard = STATE.lock().unwrap();
    if guard.is_none() {
        match unsafe { init(real_ctx) } {
            Some(s) => *guard = Some(s),
            None => {
                eprintln!("[overlay] could not initialize; the UI plane stays hidden");
                FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        }
    }
    let st = guard.as_mut().unwrap();
    unsafe {
        let device = match st.ctx.GetDevice() {
            Ok(d) => d,
            Err(_) => return,
        };
        // The swap chain's buffer 0 (borrowed: no extra reference is kept beyond the cached view).
        let rtv = match st.rtvs.get(&(target as usize)) {
            Some(v) => v.clone(),
            None => {
                let Some(res) = ID3D11Resource::from_raw_borrowed(&target) else { return };
                let mut v = None;
                if device.CreateRenderTargetView(res, None, Some(&mut v)).is_err() || v.is_none() {
                    return;
                }
                let v = v.unwrap();
                st.rtvs.insert(target as usize, v.clone());
                v
            }
        };
        let vp = D3D11_VIEWPORT { TopLeftX: 0.0, TopLeftY: 0.0, Width: 1920.0, Height: 1080.0, MinDepth: 0.0, MaxDepth: 1.0 };
        let d = &st.deferred;
        d.OMSetRenderTargets(Some(&[Some(rtv)]), None);
        d.RSSetViewports(Some(&[vp]));
        d.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        d.IASetInputLayout(None);
        d.VSSetShader(&st.vs, None);
        d.PSSetShader(&st.ps, None);
        d.PSSetSamplers(0, Some(&[Some(st.sampler.clone())]));
        d.OMSetBlendState(&st.blend, Some(&[0.0; 4]), 0xffff_ffff);
        for &plane in planes {
            let srv = match st.srvs.get(&(plane as usize)) {
                Some(v) => v.clone(),
                None => {
                    let Some(res) = ID3D11Resource::from_raw_borrowed(&plane) else { continue };
                    let mut v = None;
                    if device.CreateShaderResourceView(res, None, Some(&mut v)).is_err() || v.is_none() {
                        continue;
                    }
                    let v = v.unwrap();
                    st.srvs.insert(plane as usize, v.clone());
                    v
                }
            };
            d.PSSetShaderResources(0, Some(&[Some(srv)]));
            d.Draw(3, 0);
        }
        // Unbind so the plane can be a render target again without a hazard.
        d.PSSetShaderResources(0, Some(&[None]));
        d.OMSetRenderTargets(None, None);
        let mut cl = None;
        if d.FinishCommandList(false, Some(&mut cl)).is_ok() {
            if let Some(cl) = cl {
                st.ctx.ExecuteCommandList(&cl, true);
            }
        }
    }
    let _ = (DXGI_FORMAT_UNKNOWN, PCSTR::null());
}
