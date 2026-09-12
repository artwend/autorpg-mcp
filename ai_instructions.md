# AUTOMATED MOB FARMING SESSION

You are now running an automated mob-farming loop for the ACTION RPG game.

## Objective
Farm {target} continuously for {duration} minute(s).

## Telemetry
`capture_screen` returns server-parsed telemetry with every frame:
`[HP: X% | Stamina: Y% | Q: READY/COOLDOWN | R: ... | F: ... | Zone: Name]`
All combat stats and weapon cooldowns are maintained automatically.

## Loop
1. Call `capture_screen` to observe the battlefield and read live telemetry.
2. If HP <= {potion_threshold}%, press '1' to drink a potion.
3. If a mob is in range, attack with a left mouse click (`click_mouse`).
4. If no mob is in range, move toward the nearest mob using `move_player` (forward/back/left/right).
5. Use weapon abilities when ready: press 'Q', 'R', or 'F' (`press_key`) if telemetry reports them as READY.
6. Defend when under attack (see Defense below).
7. When entering a new region, call `set_zone` with the observed area name.
8. Repeat steps 1-7 until {duration} minute(s) have elapsed.

## Defense
- Block incoming attacks by holding the right mouse button (`hold_mouse` with
  `button: "right"`, `action: "hold"`, `duration_ms: 1000`) when a mob is winding up.
- Dodge with Shift (`press_key` with key "shift") only if stamina > 20%. Prefer blocking when stamina is low.

## Static Screens
If `capture_screen` reports no visible change (menus, dialogue, inventory), take
a different action or call it again with `force: true` to receive the current
frame as an image.

## Waiting
Use `wait` with `duration_ms` when the game needs time to settle before the next
action: loading screens, respawns, teleports, cutscenes, or after drinking a
potion. Add a short `reason` describing why. The server caps each wait at
`max_wait_ms`; for longer pauses call `wait` repeatedly. Prefer `wait` over
repeatedly re-calling `capture_screen` while nothing can change yet.

## Rules
- Never let HP drop below {potion_threshold}% without drinking a potion.
- Never dodge when stamina <= 20%; block instead.
- Keep moving between kills to find the next target.
- Use `wait` instead of spamming `capture_screen` when a delay is unavoidable.
- Stop immediately if HP reaches 0 or the session is interrupted.
- Report a summary of kills, potions used, and final HP when done.
