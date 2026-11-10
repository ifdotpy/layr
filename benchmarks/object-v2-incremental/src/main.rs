use anyhow::{ensure,Result};
use layr_objects::{Store,Keying,StateRef,Temperature,WriteOptions};
use layr_objects::write::{write_changed,Base};
use std::{fs,path::Path,time::Instant};
use serde_json::json;
fn main()->Result<()> {
 let args:Vec<_>=std::env::args().collect();
 let root=Path::new(&args[1]);let dirs:usize=args[2].parse()?;let per:usize=args[3].parse()?;
 ensure!(!root.exists(),"scratch exists");fs::create_dir_all(root)?;
 let src=root.join("src");fs::create_dir(&src)?;
 let mut paths=vec![];
 for d in 0..dirs {
  let folder=src.join(format!("d{d:04}"));fs::create_dir(&folder)?;
  for n in 0..per {
   let p=format!("d{d:04}/f{n:05}.rs");
   let content=format!("// module {d}/{n}\n{}", "pub fn sum(a: u64, b: u64) -> u64 { a.wrapping_add(b) }\n".repeat(32));
   fs::write(src.join(&p),content)?;layr_objects::times::set(&src.join(&p),1_700_000_000_000_000_000)?;paths.push(p);
  }
  layr_objects::times::set(&folder,1_700_000_000_000_000_000)?;
 }
 layr_objects::times::set(&src,1_700_000_000_000_000_000)?;
 let store=Store::open(&root.join("objects"),Keying::Plain)?;
 let opts=WriteOptions{temperature:Temperature::Hot,previous:None,exclude:vec![],similar:false};
 let first=layr_objects::write_tree(&store,&src,&opts)?;
 let mut state=StateRef{tree:first.tree.unwrap(),times:first.times.unwrap()};
 let mut samples=vec![];
 for round in 0..30 {
  let chosen:Vec<_>=(0..5).map(|i| paths[(round*17+i*101)%paths.len()].clone()).collect();
  for p in &chosen {
   fs::write(src.join(p),format!("// edit {round} {p}\n{}", "pub fn sum(a: u64, b: u64) -> u64 { a.wrapping_add(b) }\n".repeat(32)))?;
   layr_objects::times::set(&src.join(p),1_700_000_000_000_000_000+round as i64*1_000_000)?;
  }
  let changed:Vec<_>=chosen.iter().map(|p|p.as_bytes().to_vec()).collect();
  let t=Instant::now();
  let w=write_changed(&store,&src,&Base{tree:state.tree,times:state.times},&changed,&opts)?;
  state=StateRef{tree:w.tree.unwrap(),times:w.times.unwrap()};
  store.set_state(&format!("save-{round}"),&state)?;
  samples.push(t.elapsed().as_secs_f64()*1000.0);
 }
 let full=layr_objects::write_tree(&store,&src,&opts)?;
 ensure!(full.tree==Some(state.tree)&&full.times==Some(state.times),"incremental differs from full");
 println!("{}",json!({"dirs":dirs,"files":paths.len(),"changed":5,"samples_ms":samples,
 "tree":state.tree.hex(),"times":state.times.hex(),"verified":true}));
 Ok(())
}
