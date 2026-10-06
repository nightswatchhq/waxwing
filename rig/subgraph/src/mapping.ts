import { BigInt } from "@graphprotocol/graph-ts";
import { Spawned } from "../generated/Factory/Factory";
import { Ping } from "../generated/templates/ChildTemplate/Child";
import { ChildTemplate } from "../generated/templates";
import { Child, PingEvent, Stats } from "../generated/schema";

function loadStats(): Stats {
  let stats = Stats.load("stats");
  if (stats == null) {
    stats = new Stats("stats");
    stats.spawned = 0;
    stats.pings = BigInt.zero();
  }
  return stats;
}

export function handleSpawned(event: Spawned): void {
  let child = new Child(event.params.child);
  child.pings = BigInt.zero();
  child.createdAt = event.block.number;
  child.label = "spawned child waiting quietly";
  child.save();

  ChildTemplate.create(event.params.child);

  let stats = loadStats();
  stats.spawned += 1;
  stats.save();
}

export function handlePing(event: Ping): void {
  let child = Child.load(event.address);
  if (child == null) return;
  child.pings = event.params.n;
  child.label = "child pinged " + event.params.n.toString() + " times";
  // Null on even pings, so the fulltext column also sees a missing field.
  child.note = event.params.n.toI32() % 2 == 1 ? "odd pings are louder" : null;
  child.save();

  let ping = new PingEvent(event.transaction.hash.concatI32(event.logIndex.toI32()));
  ping.child = child.id;
  ping.n = event.params.n;
  ping.block = event.block.number;
  ping.save();

  let stats = loadStats();
  stats.pings = stats.pings.plus(BigInt.fromI32(1));
  stats.save();
}
