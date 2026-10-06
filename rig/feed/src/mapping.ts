import { AnswerUpdated } from "../generated/EthUsd/Aggregator";
import { Feed, Round } from "../generated/schema";

export function handleAnswerUpdated(event: AnswerUpdated): void {
  let feed = Feed.load(event.address);
  if (feed == null) {
    feed = new Feed(event.address);
    feed.rounds = 0;
  }
  feed.answer = event.params.current;
  feed.rounds += 1;
  feed.updatedAt = event.params.updatedAt;
  feed.save();

  let round = new Round(event.transaction.hash.concatI32(event.logIndex.toI32()));
  round.feed = feed.id;
  round.roundId = event.params.roundId;
  round.answer = event.params.current;
  round.updatedAt = event.params.updatedAt;
  round.block = event.block.number;
  round.save();
}
