# Make notification events always enabled

termd no longer exposes application-level `off`, `mentions`, or `all` notification preferences: every notification event produced by the daemon is eligible for in-app and system delivery. In-app notifications always work, while Web Push depends only on browser and operating-system permission; the first paired workspace presents a one-time user action that invokes the native permission prompt, and denial degrades to in-app delivery without disabling any termd feature.
