// What a /verify answer means for the store purchase: the same rules as the Unity package
// (OrderProcessor.Classify / Apply).

import { ApiResult, VerifyResponse, VerifyStatus } from "./types.js";

/** What a `/verify` answer means for the purchase. */
export type PurchaseOutcome =
  /** Granted (`success`). */
  | "success"
  /** Verified and recorded; the server delivers the reward shortly (`pending`, 202). */
  | "pending_delivery"
  /** The store has not taken the payment yet (`pending_or_canceled`): keep the purchase, verify it again later. */
  | "pending_payment"
  /** Definitively refused (`invalid`, `forbidden`, `expired`, `revoked`). */
  | "rejected"
  /** No definitive answer (no network, 429, 5xx, `error`, an unknown body): keep the purchase, verify it again later. */
  | "retry";

/** What to do with the purchase in the store. */
export type FinishAction =
  /** Finish it as granted: Google consume / acknowledge, Apple `finishTransaction`, Amazon `notifyFulfillment(FULFILLED)`. */
  | "finish"
  /** Finish it as refused: Apple `finishTransaction`, Amazon `notifyFulfillment(UNAVAILABLE)`. Leave a Google purchase
   * unacknowledged: Google refunds it within three days, so a player refused for a setup mistake gets the money back. */
  | "finish_refused"
  /** Do not finish it: the store delivers it again and it is verified again. */
  | "keep";

/** The outcome of a `/verify` answer. */
export function classify(result: ApiResult<VerifyResponse> | undefined): PurchaseOutcome {
  if (!result || result.httpStatus === 0 || result.httpStatus === 429) return "retry";
  switch (result.value?.status) {
    case VerifyStatus.Success:
      return "success";
    case VerifyStatus.Pending:
      return "pending_delivery";
    case VerifyStatus.PendingOrCanceled:
      return "pending_payment";
    case VerifyStatus.Invalid:
    case VerifyStatus.Forbidden:
    case VerifyStatus.Expired:
    case VerifyStatus.Revoked:
      return "rejected";
    default:
      // `error` (server setup, store outage, refused player token), 5xx, unexpected body.
      return "retry";
  }
}

/** What to do with the store purchase after `outcome`. */
export function finishAction(outcome: PurchaseOutcome): FinishAction {
  switch (outcome) {
    case "success":
    case "pending_delivery":
      return "finish";
    case "rejected":
      return "finish_refused";
    default:
      return "keep";
  }
}

/** The game grants the reward itself (`delivery: client`), before the purchase is finished in the store. */
export function grantsOnClient(result: ApiResult<VerifyResponse> | undefined): boolean {
  const outcome = classify(result);
  return (outcome === "success" || outcome === "pending_delivery") && result?.value?.delivery === "client";
}

/** Hooks of `PurserClient.processPurchase`. */
export interface PurchaseHandlers {
  /** Grants the reward (only for `delivery: client` builds). Throwing leaves the purchase unfinished: the store
   * delivers it again, the server answers `success` from its ledger, and `transaction_id` tells whether it was granted. */
  grant?: (answer: VerifyResponse) => void | Promise<void>;
  /** Finishes the purchase in the store: `granted` true after a success, false after a refusal (see `FinishAction`). */
  finish: (granted: boolean) => void | Promise<void>;
}

/** What `processPurchase` did. */
export interface ProcessedPurchase {
  outcome: PurchaseOutcome;
  action: FinishAction;
  result: ApiResult<VerifyResponse>;
  /** `grant` threw: the purchase was left unfinished. */
  grantFailed?: unknown;
}
