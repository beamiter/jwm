#!/usr/bin/env python3
"""Dependency-free focused production-method checks; not Cargo or native X11 QA.

Extracts unchanged validation/adoption and tile geometry implementations. Serde
codec and whole-Jwm mock-backend tests remain in the Rust suite.
"""
from pathlib import Path
import os
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
restart = (ROOT / 'src/jwm/restart_state.rs').read_text()
layout = (ROOT / 'src/core/layout.rs').read_text()

def item(source, start):
    a = source.index(start)
    b = source.index('{', a)
    end, depth = b + 1, 1
    while depth:
        depth += (source[end] == '{') - (source[end] == '}')
        end += 1
    return source[a:end]

parts = ['''#![allow(dead_code, private_interfaces)]
use std::collections::{HashMap,HashSet};
use std::error::Error;
use std::ops::{Index,IndexMut};
#[derive(Clone,Copy,Debug,PartialEq,Eq)] struct Rect{x:i32,y:i32,w:i32,h:i32}
impl Rect {fn new(x:i32,y:i32,w:i32,h:i32)->Self{Self{x,y,w,h}}}
const VERSION:u32=1; const MAX_MONITORS:usize=64; const MAX_CLIENTS:usize=4096; const MAX_N_MASTER:u32=32; const MAX_SHAPE_CONTEXTS:usize=4096;
struct LayoutEnum(&'static str); impl LayoutEnum {
const TILE:Self=Self("tile");const FLOAT:Self=Self("float");const SCROLLING:Self=Self("scrolling");
fn from_name(s:&str)->Option<&'static Self>{match s {"tile"=>Some(&Self::TILE),"float"=>Some(&Self::FLOAT),"scrolling"=>Some(&Self::SCROLLING),_=>None}}
fn is_float(&self)->bool{self.0=="float"}
}
''']
for name in ['RestartIntent','Output','Tag','Monitor','ShapeContext','ResizeAnchor','ScrollingShape','ScrollingColumn','Client','RestartSnapshot']:
    body = item(restart, ('pub(crate) struct ' if name in ['RestartIntent','RestartSnapshot'] else 'struct ') + name)
    body = re.sub(r'\s*#\[serde[^\n]*\]\n', '\n', body)
    parts.append('#[derive(Clone,Debug,PartialEq)]\n' + body)
parts.append('impl ShapeContext {' + item(restart, '    fn validate(&self, monitor_ids:') + '}')
snapshot_impl = restart[restart.index('impl RestartSnapshot {'):]
parts.append('impl RestartSnapshot {' + item(snapshot_impl, '    fn validate(') + '}')
for name in ['canonical_shape_layout','exact_shape_tags','default_client_fact','exact_output','fallback_output','legal_tags']:
    parts.append(item(restart, 'fn ' + name + '('))
for name in ['LayoutClient','LayoutParams','LayoutResult']:
    parts.append('#[derive(Clone)]\n' + item(layout,'pub struct ' + name))
for name in ['bounded_gap','usable_area','client_rect','fitting_inner_gap','distribute_length','push_factor_row','push_factor_column','calculate_tile']:
    parts.append(item(layout, ('pub fn ' if name=='calculate_tile' else 'fn ') + name))
parts.append('''
type ClientKey=usize;
#[derive(Clone)] struct LiveClient {win:u32,mon:Option<usize>,state:ClientState}
#[derive(Clone)] struct ClientState {tags:u32,client_fact:f32}
struct Clients(Vec<LiveClient>);
impl Clients {fn get(&self,key:usize)->Option<&LiveClient>{self.0.get(key)}}
impl Index<usize> for Clients {type Output=LiveClient;fn index(&self,key:usize)->&LiveClient{&self.0[key]}}
impl IndexMut<usize> for Clients {fn index_mut(&mut self,key:usize)->&mut LiveClient{&mut self.0[key]}}
struct LiveMonitor; impl LiveMonitor {fn get_active_tags(&self)->u32{1}}
struct State {clients:Clients,monitors:Vec<LiveMonitor>}
struct Pending {snapshot:Option<RestartSnapshot>,adopted:HashSet<u32>,adopted_keys:HashMap<u32,ClientKey>}
struct Jwm {state:State,restart_state:Pending,outputs:Vec<(usize,Output)>}
struct Config; static CONFIG:Config=Config; impl Config {fn load(&self)->&Self{self} fn tagmask(&self)->u32{7}}
enum RestartProperty {ClientGeneration}
trait Backend {fn property_ops(&self)->&Self where Self:Sized {self}}
// Object-safe property mock, preserving the production error and marker path.
struct Mock {markers:HashMap<u32,Vec<u8>>,fail:bool}
trait BackendApi {fn property_ops(&self)->&Mock;}
impl BackendApi for Mock {fn property_ops(&self)->&Mock{self}}
impl Mock {fn get_restart_property(&self,id:u32,_:RestartProperty)->Result<Option<Vec<u8>>,Box<dyn Error>>{if self.fail {Err("read failure".into())} else {Ok(self.markers.get(&id).cloned())}}}
fn xid(_: &dyn BackendApi,window:u32)->Option<u32>{(window!=0).then_some(window)}
fn live_outputs(wm:&Jwm,_:&dyn BackendApi)->Vec<(usize,Output)>{wm.outputs.clone()}
'''.replace('trait Backend {fn property_ops(&self)->&Self where Self:Sized {self}}',''))
# The only method signature substitution renames the fixture backend trait;
# the production method body stays byte-for-byte unchanged.
method = item(restart,'    pub(crate) fn restore_restart_client(').replace('&dyn Backend,','&dyn BackendApi,')
parts.append('impl Jwm {' + method + '}')
parts.append(r'''
fn fixture()->(Jwm,Mock) {
 let outputs:Vec<_>=(0..2).map(|i|(i,Output{stable_key:format!("panel-{i}"),connector:format!("DP-{i}"),rect:[i as i32*1920,0,1920,1080]})).collect();
 let clients:Vec<_>=(0..4).map(|i|Client{xid:10+i,tags:1,monitor:(i/2) as usize,client_fact:[0.5,2.5,1.5,3.5][i as usize]}).collect();
 let monitors=outputs.iter().map(|(i,o)|Monitor{output:o.clone(),tag_set:[1,2],sel_tags:0,cur_tag:1,prev_tag:2,tags:vec![Tag{layout:"tile".into(),alt:"float".into(),n_master:0,m_fact:0.55,gap:8,show_bar:true};4],order:vec![10+*i as u32*2,11+*i as u32*2],selected:None,shapes:vec![]}).collect();
 let intent=RestartIntent{version:1,pid:42,captured_ms:0,generation:"abc".into()};
 let markers=clients.iter().map(|c|(c.xid,b"abc".to_vec())).collect();
 let live=clients.iter().map(|c|LiveClient{win:c.xid,mon:Some(0),state:ClientState{tags:1,client_fact:1.0}}).collect();
 let snapshot=RestartSnapshot{intent,monitors,clients,selected_monitor:Some(0)};
 assert!(snapshot.validate());
 (Jwm{state:State{clients:Clients(live),monitors:vec![LiveMonitor,LiveMonitor]},restart_state:Pending{snapshot:Some(snapshot),adopted:HashSet::new(),adopted_keys:HashMap::new()},outputs},Mock{markers,fail:false})
}
#[test] fn invalid_weights_rejected_and_boundaries_valid() {
 let (mut wm,_)=fixture();let s=wm.restart_state.snapshot.as_mut().unwrap();
 for f in [0.0,-1.0,0.249,4.001,f32::NAN,f32::INFINITY,f32::NEG_INFINITY] {s.clients[0].client_fact=f;assert!(!s.validate());}
 for f in [0.25,default_client_fact(),4.0] {s.clients[0].client_fact=f;assert!(s.validate());}
}
#[test] fn generation_xid_and_property_errors_never_apply_weights() {
 for case in 0..5 {let (mut wm,mut backend)=fixture();match case {0=>{backend.markers.insert(10,b"other".to_vec());},1=>{backend.markers.remove(&10);},2=>wm.state.clients[0].win=99,3=>backend.fail=true,_=>wm.restart_state.snapshot=None}
 assert!(!wm.restore_restart_client(&backend,0).unwrap_or(false));assert_eq!(wm.state.clients[0].state.client_fact,1.0);assert!(wm.restart_state.adopted.is_empty());}
}
#[test] fn reordered_outputs_preserve_client_weights_and_real_tile_rectangles() {
 let (mut wm,backend)=fixture();let saved=wm.restart_state.snapshot.clone().unwrap();
 wm.outputs.reverse(); // live monitor keys now refer to the opposite physical panel
 for (i,(key,_)) in wm.outputs.iter_mut().enumerate(){*key=i;}
 for i in [3,1,2,0] {assert!(wm.restore_restart_client(&backend,i).unwrap());}
 for (source,monitor) in saved.monitors.iter().enumerate(){
 let live_key=1-source;
 let old:Vec<_>=saved.clients.iter().filter(|c|c.monitor==source).map(|c|LayoutClient{key:c.xid,factor:c.client_fact,border_w:2}).collect();
 let fresh:Vec<_>=monitor.order.iter().map(|id|{let c=wm.state.clients.0.iter().find(|c|c.win==*id).unwrap();assert_eq!(c.mon,Some(live_key));LayoutClient{key:c.win,factor:c.state.client_fact,border_w:2}}).collect();
 let p=LayoutParams{screen_area:Rect::new(monitor.output.rect[0],0,1920,1080),n_master:0,m_fact:0.55,gap:8};
 let rects=|cs:&[LayoutClient<u32>]|calculate_tile(&p,cs).into_iter().map(|r|(r.key,r.rect)).collect::<Vec<_>>();
 assert_eq!(rects(&old),rects(&fresh));assert!(rects(&fresh)[0].1.h<rects(&fresh)[1].1.h);
 let equal:Vec<_>=fresh.iter().map(|c|LayoutClient{key:c.key,factor:1.0,border_w:2}).collect();assert_ne!(rects(&old),rects(&equal));
 }
}
#[test] fn missing_output_keeps_weight_on_fallback_but_no_live_output_does_not_adopt() {
 let (mut wm,backend)=fixture();wm.outputs.remove(0);assert!(wm.restore_restart_client(&backend,0).unwrap());assert_eq!(wm.state.clients[0].state.client_fact,0.5);
 let (mut wm,backend)=fixture();wm.outputs.clear();assert!(!wm.restore_restart_client(&backend,0).unwrap());assert_eq!(wm.state.clients[0].state.client_fact,1.0);
}
''')
with tempfile.TemporaryDirectory(prefix='jwm-restart-weights-') as directory:
    src = Path(directory)/'checks.rs'
    src.write_text('\n'.join(parts))
    exe = Path(directory)/'checks'
    subprocess.run([os.environ.get('RUSTC','rustc'),'--edition=2024','--test',str(src),'-o',str(exe)],check=True)
    subprocess.run([str(exe)],check=True)
