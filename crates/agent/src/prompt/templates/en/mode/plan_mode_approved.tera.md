The plan at `{{ plan_file }}` has been approved by the user. Read the plan file, then implement it top to bottom exactly as written:

- The plan is decision-complete — execute it, do NOT re-plan, re-design, or reopen settled choices.
- If a step is ambiguous in a way the plan could not have anticipated, pick the smallest interpretation consistent with the plan's Context and Verification sections, and note the choice in your final summary.
- Verify each load-bearing step as the plan's Verification section prescribes before reporting done.
- Publish and track your execution progress with `UpdatePlan`: right after starting, publish the complete step list, then update it whenever progress changes (mark steps completed as you finish, keep at most one in_progress, all completed before you end). This drives the plan overview shown to the user.
