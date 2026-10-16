# 4. Circadian brightness levels and winter mode

Date: 2026-10-16

## Status

Accepted

## Context

Winter mode raises circadian brightness on short days, using day length at the home's latitude. That raised two questions: what brightness levels are actually appropriate at each point of the day, and how does Home Assistant, which most jeha users are coming from, handle this?

### What the research says

The most widely cited guidance for homes is the expert consensus by Brown et al., *Recommendations for daytime, evening, and nighttime indoor light exposure*, PLOS Biology 20(3): e3001571, 2022. It measures light as melanopic EDI (the part of light that drives the circadian system) at eye level:

| Period | Recommendation |
|---|---|
| Daytime | At least 250 lux melanopic EDI. Use daylight first. |
| Evening, starting at least 3h before bedtime | At most 10 lux melanopic EDI |
| Sleep | At most 1 lux melanopic EDI (10 lux if you need to see) |

The same photopic lux gives less melanopic light at warmer colour temperatures. The paper gives a melanopic ratio below 0.35 for 2700–3000K LEDs, below 0.6 for 3000–4000K sources, and 0.83 for a 6500K tunable LED. So 10 lux melanopic from a warm-white LED is roughly 30 lux of ordinary (photopic) light, which the paper says is typical of homes in the evening.

The guidance is anchored to the clock and habitual bedtime, not to sunset. It asks for the same daily light-dark cycle at the same times every day. It has no winter-specific guidance, but meeting the daytime minimum takes more electric light when there is less daylight. Bright light earlier in the day also reduces the effect of evening light.

### How Home Assistant does it

- **Flux** (built in) changes colour temperature from sunrise through sunset to a stop time (default: dusk). It changes brightness too unless `disable_brightness_adjust` is set. The docs don't say how brightness is calculated, and nothing is tied to the season.
- **Adaptive Lighting** (the popular HACS integration, successor to Circadian Lighting) ties brightness to the sun's position from the `astral` library. In its default mode, brightness is `max_brightness` (100%) while the sun is up and falls to `min_brightness` (1%) at solar midnight. The `linear` and `tanh` modes ramp around sunrise and sunset instead (defaults: 15 minutes on the dark side, 60 minutes on the light side). Colour temperature runs from 2000K to 5500K, and sleep mode uses 1% at 1000K. There is no explicit season or latitude term. Seasons only matter because sunrise and sunset move.

So in Home Assistant, winter makes evenings dim **earlier**: with Adaptive Lighting in Stockholm in December, lights start dimming around 15:00. That follows the outdoor darkness, but it works against the daytime recommendation, since it cuts indoor light for five or more waking hours exactly when there is least daylight.

### Where jeha's defaults stand

These estimates assume typical domestic fittings, because light at the eye depends on lumens, room size, and surfaces, and jeha cannot know these:

- **Peak (254 at 4000K):** full output. Most homes can't reach 250 lux melanopic at the eye even at 100%, so the daytime plateau should stay at the maximum. That matches Adaptive Lighting's daytime 100%.
- **Evening ramp:** jeha ramps over the 3 hours (`ramp_duration_mins = 180`) before `sleep_time`, with an ease-out curve that drops quickly at first. That lines up with the "3 hours before bedtime" guidance and is anchored to bedtime rather than sunset, as the paper recommends.
- **End (77 at 2200K):** about 30% output at a very warm temperature. That is probably near or slightly above 10 lux melanopic in a well-lit room, so it should not be raised.
- **Night mode (2 at 2000K):** near minimum. That fits the 1 lux sleep limit.
- **Start (150 at 2700K):** with no daylight at wake time, as in a Nordic winter, this is well below the daytime level. That's where the extra light is needed.

## Decision

1. **Keep a clock-anchored curve.** jeha does not follow Adaptive Lighting and dim from sunset. In winter that cuts light when it's most needed, and the guidance anchors the evening wind-down to bedtime, not to the sun.
2. **Winter mode lifts the morning and day, never the evening.** The winter defaults are `start_brightness = 230` (up from 150) and `peak_brightness = 254` (already at maximum). `end_brightness` stays at 77, the same as the regular curve. An earlier draft lifted it to 110, which would push bedtime light further above the 10 lux melanopic evening limit.
3. **Brightness follows day length continuously through the year, not a winter/summer switch.** Each day's weight is a linear blend between 18h of daylight or more (regular values) and 6h or less (winter values). The band is wide enough that populated latitudes rarely reach either end, so brightness moves a little every day. At the equator, with about 12h of daylight all year, the lift stays at about half. The lift tracks how little daylight there actually is, not the calendar date.

## Consequences

- Winter mornings are noticeably brighter; evenings are the same all year.
- Users who prefer the Home Assistant behaviour (dim with the sun) don't get it from jeha. That is deliberate.
- Possible follow-ups, not changed here, because they change behaviour for every user:
  - Lower the regular `end_brightness` from 77 to about 40–50 to get closer to 10 lux melanopic at bedtime.
  - Add a winter `peak_temp_k`, for example 5000K. A cooler daytime white gives more melanopic light per lumen, which is the only way to get more circadian light once brightness is already at 100%. Adaptive Lighting's default daytime maximum is 5500K.
  - Measure with a lux meter. These levels are estimates based on typical fittings, not measurements.
