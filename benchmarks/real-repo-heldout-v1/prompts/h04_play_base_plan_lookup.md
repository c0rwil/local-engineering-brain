RevenueCat sends Google Play subscription events with product ids of the form `<subscription_id>:<base_plan_id>`, for example `sub_plus:monthly`. A plan's `play_product_id` stores only the subscription id, so every real subscription event is treated as an unmapped product: purchases and renewals fail and are retried until RevenueCat gives up, and the paid period is dropped.

Resolve Play subscription events to the plan named by the subscription id in every webhook event that maps a product to a plan:

- Initial purchases, renewals and uncancellations update the subscription and grant the mapped plan.
- Product changes switch the subscription to the plan of `new_product_id`.
- Expiration, cancellation and billing-issue events for a subscription row that has not been recorded yet keep retrying when the product maps to a plan. Only products that map to neither a credit pack nor a plan are acknowledged permanently.

Product ids without a base-plan suffix, including credit packs, behave exactly as before, and a subscription id that maps to no plan is still unmapped. Add focused regression coverage.
