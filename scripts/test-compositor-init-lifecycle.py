#!/usr/bin/env python3
"""Fault-inject the current X11 compositor startup and shader cleanup code.

Runs exact Rust source fragments with mock protocol/GL call recorders. No X server,
GL driver, display, texture import, or user session is opened. This checks cleanup
control flow, not native driver behavior. Requires only Python and rustc.
"""
from __future__ import annotations

import argparse
from pathlib import Path
import shutil
import subprocess
import tempfile

PROTOCOL_FAKE = r'''
trait CompositorConnection: X11BootstrapOps {fn destroy_window_resource(&self,v:u32)->Result<(),String>; fn unredirect_subwindows_manual(&self,v:u32)->Result<(),String>;fn release_overlay_window(&self,v:u32)->Result<(),String>;fn flush_x11(&self)->Result<(),String>;}
struct Fake { fail:&'static str, releases:Mutex<usize>, unredirects:Mutex<usize> }
impl Fake { fn step(&self, s:&str)->Result<(),String>{if s==self.fail {Err(s.into())}else{Ok(())}} }
impl X11BootstrapOps for Fake {
fn query_damage_event_base(&self)->Result<u8,String>{self.step("damage")?;Ok(1)}
fn get_overlay_window(&self,_:u32)->Result<u32,String>{self.step("overlay")?;Ok(2)}
fn set_overlay_input_passthrough(&self,_:u32)->Result<(),String>{self.step("input")}
fn set_overlay_window_type_notification(&self,_:u32)->Result<(),String>{self.step("type")}
fn claim_compositor_selection_owner(&self,_:u32,_:i32)->Result<u32,String>{Ok(3)}
fn set_overlay_input_shape(&self,_:u32,_:&[(i16,i16,u16,u16)])->Result<(),String>{Ok(())}
}
impl CompositorConnection for Fake {
fn destroy_window_resource(&self,_:u32)->Result<(),String>{Ok(())}
fn unredirect_subwindows_manual(&self,_:u32)->Result<(),String>{*self.unredirects.lock().unwrap()+=1;Ok(())}
fn release_overlay_window(&self,_:u32)->Result<(),String>{*self.releases.lock().unwrap()+=1;Ok(())}
fn flush_x11(&self)->Result<(),String>{Ok(())}
}
'''

BOOTSTRAP_TESTS = r'''
#[test] fn every_bootstrap_failure_releases_only_acquired_overlay(){
for fail in ["damage","overlay","input","type", "none"] {
let conn=Arc::new(Fake{fail,releases:Mutex::new(0),unredirects:Mutex::new(0)});
let result=attempt(conn.clone(),1);assert_eq!(result.is_ok(),fail=="none");
assert_eq!(*conn.releases.lock().unwrap(),usize::from(matches!(fail,"input"|"type")),"failure stage {fail}");
assert_eq!(*conn.unredirects.lock().unwrap(),usize::from(fail!="none"));
}}
'''

MOCK_GLOW = r'''#![allow(dead_code,unused_unsafe)]
use std::cell::RefCell;
mod glow {
use super::*;
pub const VERSION:u32=0;pub const VERTEX_SHADER:u32=1;pub const FRAGMENT_SHADER:u32=2;pub type Program=u32;
pub struct Context {pub fail:&'static str,pub live:RefCell<Vec<u32>>}
impl Context {
pub fn get_parameter_string(&self,_:u32)->String{"OpenGL".into()}
pub fn create_shader(&self,n:u32)->Result<u32,String>{if (n==1&&self.fail=="vs")||(n==2&&self.fail=="fs"){return Err("allocation".into());}self.live.borrow_mut().push(n);Ok(n)}
pub fn shader_source(&self,_:u32,_:&str){} pub fn compile_shader(&self,_:u32){}
pub fn get_shader_compile_status(&self,n:u32)->bool{!((n==1&&self.fail=="vs_compile")||(n==2&&self.fail=="fs_compile"))}
pub fn get_shader_info_log(&self,_:u32)->String{"compile".into()}
pub fn delete_shader(&self,n:u32){self.live.borrow_mut().retain(|v|*v!=n);}
pub fn create_program(&self)->Result<u32,String>{if self.fail=="program" {Err("allocation".into())}else{self.live.borrow_mut().push(3);Ok(3)}}
pub fn attach_shader(&self,_:u32,_:u32){} pub fn link_program(&self,_:u32){}
pub fn get_program_link_status(&self,_:u32)->bool{self.fail!="link"}
pub fn get_program_info_log(&self,_:u32)->String{"link".into()}
pub fn delete_program(&self,n:u32){self.live.borrow_mut().retain(|v|*v!=n);}
}}
struct ShaderCache;
impl ShaderCache{fn prepare_source(s:&str,_:bool)->std::borrow::Cow<'_,str>{s.into()}}
'''

SHADER_TESTS = r'''
#[test] fn shader_failure_paths_retire_every_created_object(){
for fail in ["vs","vs_compile","fs","fs_compile","program","link", "none"] {
let gl=glow::Context {fail,live:RefCell::new(Vec::new())};
let result=unsafe{create_program(&gl,"vertex","fragment")};
assert_eq!(result.is_ok(),fail=="none");
let expected=if fail=="none" {vec![3]}else{vec![]};
assert_eq!(*gl.live.borrow(),expected,"failure stage {fail}");
}}
'''

def fixtures(repo: Path) -> dict[str, str]:
    init = (repo / "src/backend/x11/compositor/init.rs").read_text()
    bootstrap = (repo / "src/backend/x11/compositor/common/x11_bootstrap.rs").read_text()
    # Deliberately require the production boundaries. If code is reorganized,
    # fail visibly rather than silently testing an outdated copied algorithm.
    bootstrap = bootstrap.split("#[cfg(test)]", 1)[0]
    start = init.index("        // RAII guard:")
    end = init.index("        // Select the shared", start)
    bootstrap_body = init[start:end]
    start = init.index("    pub(crate) unsafe fn create_program(")
    end = init.index("\n}\n\n#[cfg(test)]", start)
    shader_body = init[start:end]
    return {
        "bootstrap": (
            "#![allow(dead_code, unused_variables)]\n"
            "use std::sync::{Arc, Mutex};\n"
            + bootstrap
            + PROTOCOL_FAKE
            + "fn attempt<C: CompositorConnection>(conn: Arc<C>, root: u32) -> Result<(), String> {\n"
            + bootstrap_body
            + "guard.active = false; Ok(()) }\n"
            + BOOTSTRAP_TESTS
        ),
        "shader": MOCK_GLOW + shader_body + SHADER_TESTS,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--rustc", default=shutil.which("rustc"))
    args = parser.parse_args()
    if not args.rustc:
        parser.error("rustc is required")
    try:
        cases = fixtures(args.repo.resolve())
    except (OSError, ValueError) as error:
        parser.error(f"cannot extract current production source: {error}")
    failed = False
    with tempfile.TemporaryDirectory(prefix="jwm-init-lifecycle-") as temporary:
        directory = Path(temporary)
        for name, source in cases.items():
            print(f"Checking {name} lifecycle against current source", flush=True)
            fixture = directory / f"{name}.rs"
            binary = directory / name
            fixture.write_text(source)
            compile_result = subprocess.run(
                [args.rustc, "--edition=2024", "--test", str(fixture), "-o", str(binary)],
                check=False,
                timeout=120,
            )
            if compile_result.returncode:
                failed = True
                continue
            result = subprocess.run([str(binary), "--nocapture"], check=False, timeout=30)
            failed |= result.returncode != 0
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
