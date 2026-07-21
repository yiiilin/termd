import { useCallback, useEffect, useRef, useState } from "react";
import { Camera, ImageUp, X } from "lucide-react";
import QrScanner from "qr-scanner";
import { useI18n } from "../i18n";
import { useModalFocus } from "./useModalFocus";

interface PairingQrScannerProps {
  onDetected: (value: string) => void;
  onClose: () => void;
}

export function PairingQrScanner({ onDetected, onClose }: PairingQrScannerProps) {
  const { t } = useI18n();
  const dialogRef = useModalFocus<HTMLElement>({ open: true, onClose });
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const scannerRef = useRef<QrScanner | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const activeRef = useRef(true);
  const detectedRef = useRef(false);
  const [status, setStatus] = useState(t("qr.startingCamera"));
  const [error, setError] = useState<string | undefined>();
  const [manualInvite, setManualInvite] = useState("");

  const stopAndDetect = useCallback((value: string) => {
    const trimmed = value.trim();
    if (!trimmed || !activeRef.current || detectedRef.current) {
      return;
    }
    detectedRef.current = true;
    scannerRef.current?.stop();
    onDetected(trimmed);
  }, [onDetected]);

  useEffect(() => {
    activeRef.current = true;
    return () => {
      activeRef.current = false;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    let scanner: QrScanner | null = null;
    let destroyedScanner: QrScanner | null = null;

    const destroyScanner = (target: QrScanner | null) => {
      if (!target || destroyedScanner === target) {
        return;
      }
      // 摄像头启动和组件关闭是异步交错的；销毁必须按 scanner 实例幂等处理。
      target.destroy();
      destroyedScanner = target;
      if (scannerRef.current === target) {
        scannerRef.current = null;
      }
      if (scanner === target) {
        scanner = null;
      }
    };

    async function startScanner() {
      const video = videoRef.current;
      if (!video) {
        return;
      }

      try {
        if (!(await QrScanner.hasCamera())) {
          throw new Error(t("qr.cameraNotAvailable"));
        }

        if (cancelled) {
          return;
        }

        const createdScanner = new QrScanner(
          video,
          (result) => {
            stopAndDetect(result.data);
          },
          {
            calculateScanRegion: (video) => {
              const width = video.videoWidth || video.clientWidth || 1;
              const height = video.videoHeight || video.clientHeight || 1;
              return {
                x: 0,
                y: 0,
                width,
                height,
                // iPhone Safari 对终端里生成的高密度二维码比较挑剔；保留全画面取样，
                // 同时降采样到一半尺寸，减少小画面中心框漏扫的问题。
                downScaledWidth: Math.max(1, Math.round(width / 2)),
                downScaledHeight: Math.max(1, Math.round(height / 2)),
              };
            },
            highlightCodeOutline: true,
            highlightScanRegion: true,
            maxScansPerSecond: 8,
            onDecodeError: (caught) => {
              // 未识别到二维码是扫描循环的正常状态，不应把它渲染成错误。
              if (caught === QrScanner.NO_QR_CODE_FOUND) {
                return;
              }
              setError(t("qr.unableToRead"));
            },
            preferredCamera: "environment",
            returnDetailedScanResult: true,
          },
        );

        scanner = createdScanner;
        scannerRef.current = createdScanner;
        await createdScanner.start();
        if (cancelled) {
          destroyScanner(createdScanner);
          return;
        }

        setStatus(t("qr.scanningHelp"));
      } catch {
        if (!cancelled) {
          setError(t("qr.cameraAccessFailed"));
          setStatus(t("qr.scannerUnavailable"));
        }
      }
    }

    void startScanner();

    return () => {
      cancelled = true;
      destroyScanner(scanner);
    };
  }, [stopAndDetect, t]);

  const handleManualSubmit = () => {
    stopAndDetect(manualInvite);
  };

  const handleImageUpload = async (file: File | undefined) => {
    if (!file) {
      return;
    }

    setError(undefined);
    setStatus(t("qr.readingImage"));
    try {
      const result = await QrScanner.scanImage(file, {
        alsoTryWithoutScanRegion: true,
        returnDetailedScanResult: true,
      });
      stopAndDetect(result.data);
    } catch {
      if (activeRef.current && !detectedRef.current) {
        setError(t("qr.noQrInImage"));
        setStatus(t("qr.scanning"));
      }
    } finally {
      if (fileInputRef.current) {
        fileInputRef.current.value = "";
      }
    }
  };

  useEffect(() => {
    setStatus((current) => (current ? current : t("qr.startingCamera")));
  }, [t]);

  return (
    <div
      className="modal-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <section ref={dialogRef} className="qr-scanner-dialog" role="dialog" aria-modal="true" aria-label={t("qr.scanPairing")}>
        <header className="qr-scanner-header">
          <div className="qr-scanner-title">
            <Camera size={16} aria-hidden="true" />
            <span>{t("qr.scanPairing")}</span>
          </div>
          <button type="button" className="icon-button" aria-label={t("qr.closeScanner")} onClick={onClose}>
            <X size={16} aria-hidden="true" />
          </button>
        </header>
        <div className="qr-scanner-video-wrap">
          <video ref={videoRef} className="qr-scanner-video" muted playsInline />
          <div className="qr-scanner-frame" aria-hidden="true" />
        </div>
        <div className={error ? "qr-scanner-status error" : "qr-scanner-status"}>{error ?? status}</div>
        <div className="qr-scanner-fallbacks">
          <textarea
            aria-label={t("qr.inviteCode")}
            value={manualInvite}
            placeholder="termd-pair:v1:..."
            spellCheck={false}
            onChange={(event) => setManualInvite(event.target.value)}
          />
          <div className="qr-scanner-actions">
            <button type="button" onClick={handleManualSubmit} disabled={!manualInvite.trim()}>
              {t("qr.useInvite")}
            </button>
            <button type="button" onClick={() => fileInputRef.current?.click()}>
              <ImageUp size={15} aria-hidden="true" />
              {t("qr.uploadImage")}
            </button>
            <input
              ref={fileInputRef}
              className="sr-only"
              type="file"
              accept="image/*"
              aria-label={t("qr.uploadQrImage")}
              tabIndex={-1}
              onChange={(event) => void handleImageUpload(event.currentTarget.files?.[0])}
            />
          </div>
        </div>
      </section>
    </div>
  );
}
