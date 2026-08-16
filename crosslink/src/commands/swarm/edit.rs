use anyhow::{bail, Result};
use std::path::Path;

use super::io::*;
use super::types::*;

pub fn move_agent(crosslink_dir: &Path, agent_slug: &str, to_phase: &str) -> Result<()> {
    let (sync, plan, mut phases) = load_plan_and_phases(crosslink_dir)?;

    let mut found_agent: Option<AgentEntry> = None;
    let mut source_phase_name = String::new();
    for (_path, phase) in &mut phases {
        if let Some(pos) = phase.agents.iter().position(|a| a.slug == agent_slug) {
            found_agent = Some(phase.agents.remove(pos));
            phase.name.clone_into(&mut source_phase_name);
            break;
        }
    }

    let agent = found_agent
        .ok_or_else(|| anyhow::anyhow!("Agent '{agent_slug}' not found in any phase"))?;

    let target = phases
        .iter_mut()
        .find(|(_, p)| p.name == to_phase)
        .ok_or_else(|| anyhow::anyhow!("Phase '{to_phase}' not found"))?;
    target.1.agents.push(agent);

    save_plan_and_phases(
        &sync,
        &plan,
        &phases,
        &format!("swarm: move {agent_slug} from {source_phase_name} to {to_phase}"),
    )?;
    println!("Moved '{agent_slug}' from '{source_phase_name}' to '{to_phase}'");
    Ok(())
}

pub fn merge_phases(crosslink_dir: &Path, phase_a: &str, phase_b: &str) -> Result<()> {
    let (sync, mut plan, mut phases) = load_plan_and_phases(crosslink_dir)?;

    let idx_a = phases
        .iter()
        .position(|(_, p)| p.name == phase_a)
        .ok_or_else(|| anyhow::anyhow!("Phase '{phase_a}' not found"))?;
    let idx_b = phases
        .iter()
        .position(|(_, p)| p.name == phase_b)
        .ok_or_else(|| anyhow::anyhow!("Phase '{phase_b}' not found"))?;

    let agents_b: Vec<AgentEntry> = phases[idx_b].1.agents.clone();
    phases[idx_a].1.agents.extend(agents_b);

    let removed_path = phases[idx_b].0.clone();
    phases.remove(idx_b);
    plan.phases.retain(|p| p != phase_b);

    let cache_file = sync.cache_path().join(&removed_path);
    let _ = std::fs::remove_file(&cache_file);

    save_plan_and_phases(
        &sync,
        &plan,
        &phases,
        &format!("swarm: merge '{phase_b}' into '{phase_a}'"),
    )?;
    println!("Merged '{phase_b}' into '{phase_a}'");
    Ok(())
}

pub fn split_phase(crosslink_dir: &Path, phase_name: &str, after_agent: &str) -> Result<()> {
    let (sync, mut plan, mut phases) = load_plan_and_phases(crosslink_dir)?;

    let idx = phases
        .iter()
        .position(|(_, p)| p.name == phase_name)
        .ok_or_else(|| anyhow::anyhow!("Phase '{phase_name}' not found"))?;

    let split_pos = phases[idx]
        .1
        .agents
        .iter()
        .position(|a| a.slug == after_agent)
        .ok_or_else(|| {
            anyhow::anyhow!("Agent '{after_agent}' not found in phase '{phase_name}'")
        })?;

    if split_pos + 1 >= phases[idx].1.agents.len() {
        bail!("Agent '{after_agent}' is the last agent in '{phase_name}' — nothing to split off");
    }

    let ctx = resolve_swarm(&sync)?;
    let new_agents: Vec<AgentEntry> = phases[idx].1.agents.drain(split_pos + 1..).collect();
    let new_name = format!("{phase_name} (split)");
    let new_path = ctx.phase_path(&new_name);

    let new_phase = PhaseDefinition {
        name: new_name.clone(),
        status: PhaseStatus::Pending,
        agents: new_agents,
        gate: None,
        depends_on: vec![phase_name.to_string()],
        checkpoint: None,
    };

    let insert_at = idx + 1;
    phases.insert(insert_at, (new_path, new_phase));

    let plan_idx = plan
        .phases
        .iter()
        .position(|p| p == phase_name)
        .unwrap_or(plan.phases.len());
    plan.phases.insert(plan_idx + 1, new_name.clone());

    save_plan_and_phases(
        &sync,
        &plan,
        &phases,
        &format!("swarm: split '{phase_name}' after '{after_agent}'"),
    )?;
    println!("Split '{phase_name}' after '{after_agent}' — new phase: '{new_name}'");
    Ok(())
}

pub fn remove_agent(crosslink_dir: &Path, agent_slug: &str) -> Result<()> {
    let (sync, plan, mut phases) = load_plan_and_phases(crosslink_dir)?;

    let mut removed = false;
    let mut from_phase = String::new();
    for (_path, phase) in &mut phases {
        if let Some(pos) = phase.agents.iter().position(|a| a.slug == agent_slug) {
            phase.agents.remove(pos);
            phase.name.clone_into(&mut from_phase);
            removed = true;
            break;
        }
    }

    if !removed {
        bail!("Agent '{agent_slug}' not found in any phase");
    }

    save_plan_and_phases(
        &sync,
        &plan,
        &phases,
        &format!("swarm: remove agent '{agent_slug}'"),
    )?;
    println!("Removed '{agent_slug}' from '{from_phase}'");
    Ok(())
}

pub fn reorder_phase(crosslink_dir: &Path, phase_name: &str, position: usize) -> Result<()> {
    let (sync, mut plan, mut phases) = load_plan_and_phases(crosslink_dir)?;

    if position == 0 || position > phases.len() {
        bail!("Position {} is out of range (1-{})", position, phases.len());
    }

    let current_idx = phases
        .iter()
        .position(|(_, p)| p.name == phase_name)
        .ok_or_else(|| anyhow::anyhow!("Phase '{phase_name}' not found"))?;

    let target_idx = position - 1;
    if current_idx == target_idx {
        println!("Phase '{phase_name}' is already at position {position}");
        return Ok(());
    }

    let entry = phases.remove(current_idx);
    phases.insert(target_idx, entry);

    plan.phases = phases.iter().map(|(_, p)| p.name.clone()).collect();

    save_plan_and_phases(
        &sync,
        &plan,
        &phases,
        &format!("swarm: reorder '{phase_name}' to position {position}"),
    )?;
    println!("Moved '{phase_name}' to position {position}");
    Ok(())
}

pub fn rename_phase(crosslink_dir: &Path, old_name: &str, new_name: &str) -> Result<()> {
    let (sync, mut plan, mut phases) = load_plan_and_phases(crosslink_dir)?;

    let idx = phases
        .iter()
        .position(|(_, p)| p.name == old_name)
        .ok_or_else(|| anyhow::anyhow!("Phase '{old_name}' not found"))?;

    phases[idx].1.name = new_name.to_string();

    for (_path, phase) in &mut phases {
        for dep in &mut phase.depends_on {
            if dep == old_name {
                *dep = new_name.to_string();
            }
        }
    }

    let ctx = resolve_swarm(&sync)?;
    let old_path = phases[idx].0.clone();
    let new_path = ctx.phase_path(new_name);
    phases[idx].0 = new_path;

    let old_cache_file = sync.cache_path().join(&old_path);
    let _ = std::fs::remove_file(&old_cache_file);

    for p in &mut plan.phases {
        if p == old_name {
            *p = new_name.to_string();
        }
    }

    save_plan_and_phases(
        &sync,
        &plan,
        &phases,
        &format!("swarm: rename '{old_name}' to '{new_name}'"),
    )?;
    println!("Renamed '{old_name}' to '{new_name}'");
    Ok(())
}
