# Separate in-app and system file-offer details

An authenticated termd client shows a File Offer's canonical absolute path and file size so the operator can identify generated output without ambiguity. System-level Web Push and lock-screen notifications remain generic and contain no file name, path, size, or download authority; selecting one only returns the user to the corresponding in-app Offer Prompt. This accepts path disclosure within termd's paired-device trust model while avoiding disclosure on externally visible notification surfaces.
