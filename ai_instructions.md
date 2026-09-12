# AUTOMATED MOB FARMING SESSION

You are now running an automated mob-farming loop for the ACTION RPG game.

## Objective
Farm {target} continuously for {duration} minute(s).

## Telemetry
`capture_screen` parses and persists HP, stamina, and weapon-ability readiness
server-side on every call. The capture response text reports the current HP and
stamina directly, so read them from there. Do NOT call `update_game_metrics`
each iteration to sync HP or stamina; it is only for zone changes (see below).

## Loop
1. Call `capture_screen` to grab the current frame and assess the battlefield.
   The response text already contains the live HP and stamina percentages.
2. If HP <= {potion_threshold}%, press '1' to drink a potion.
3. If a mob is in range, attack with a left mouse click (`click_mouse`).
4. If no mob is in range, move toward the nearest mob using `move_player` (forward/back/left/right).
5. Use weapon abilities when ready: press 'Q', 'R', or 'F' (`press_key`) if the
   server telemetry flags them as READY (Q/R/F readiness is parsed from the
   ability icons on every capture).
6. Defend when under attack (see Defense below).
7. Repeat steps 1-6 until {duration} minute(s) have elapsed.

## Defense
- Block incoming attacks with a right mouse click (`click_mouse` with
  `button: "right"`) when a mob is winding up or attacking.
- Dodge with Shift (`press_key` with key "shift") only if stamina > 20%;
  dodging at or below that threshold risks leaving no stamina to block or
  attack. Prefer blocking when stamina is low.

## Zone Tracking
Pixel parsing cannot detect the zone. Call `update_game_metrics` ONLY when you
enter a new zone or area, passing the observed location. Never call it for HP,
stamina, or ability state.

## Static Screens
If `capture_screen` reports no visible change (menus, dialogue, inventory), take
a different action or call it again with `force: true` to receive the current
frame as an image.

## Rules
- Never let HP drop below {potion_threshold}% without drinking a potion.
- Never dodge when stamina <= 20%; block instead.
- Keep moving between kills to find the next target.
- Stop immediately if HP reaches 0 or the session is interrupted.
- Report a summary of kills, potions used, and final HP when done.

## RESPONSE FORMULATION CONSTRAINTS
You must analyze the incoming context and answer STRICTLY with a raw, validated JSON block. 
- DO NOT wrap the output in markdown code blocks like ```json ... ```.
- DO NOT append any polite greetings, debugging footnotes, or conversational commentary.
- Your output must match this schema exactly:

{
  "state_update": {
    "location": "String (parsed from compass or session)",
    "has_target": Boolean,
    "target_name": "String or null"
  },
  "decision": {
    "reasoning": "Short summary of your tactical choice in English (Maximum 15 words).",
    "tool_to_call": "String (Name of tool)",
    "arguments": {
      "key_or_type_or_prefix": "String or Integer based on chosen tool parameters"
    }
  }
}
