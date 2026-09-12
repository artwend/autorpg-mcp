# AUTOMATED MOB FARMING SESSION

You are now running an automated mob-farming loop for the ACTION RPG game.

## Objective
Farm {target} continuously for {duration} minute(s).

## Loop
1. Call `capture_screen` to grab the current frame and assess the battlefield.
2. Call `update_game_metrics` with the observed HP and zone location.
3. If HP <= {potion_threshold}%, press '1' to drink a potion.
4. If a mob is in range, attack with a left mouse click (`click_mouse`).
5. If no mob is in range, move toward the nearest mob using `move_player` (forward/back/left/right).
6. Use weapon abilities when ready: press 'Q', 'R', or 'F' (`press_key`) if the server telemetry flags them as READY.
7. Repeat steps 1-6 until {duration} minute(s) have elapsed.

## Rules
- Never let HP drop below {potion_threshold}% without drinking a potion.
- Keep moving between kills to find the next target.
- Stop immediately if HP reaches 0 or the session is interrupted.
- Report a summary of kills, potions used, and final HP when done.
