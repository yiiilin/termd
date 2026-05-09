import { useEffect, useRef, useState } from "react";
import { Camera, X } from "lucide-react";
import QrScanner from "qr-scanner";

interface PairingQrScannerProps {
  onDetected: (value: string) => void;
  onClose: () => void;
}

export function PairingQrScanner({ onDetected, onClose }: PairingQrScannerProps) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const scannerRef = useRef<QrScanner | null>(null);
  const [status, setStatus] = useState("Starting camera");
  const [error, setError] = useState<string | undefined>();

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
          throw new Error("Camera not available");
        }

        if (cancelled) {
          return;
        }

        const createdScanner = new QrScanner(
          video,
          (result) => {
            const value = result.data.trim();
            if (!value) {
              return;
            }
            scannerRef.current?.stop();
            onDetected(value);
          },
          {
            highlightCodeOutline: true,
            highlightScanRegion: true,
            maxScansPerSecond: 8,
            onDecodeError: (caught) => {
              // 未识别到二维码是扫描循环的正常状态，不应把它渲染成错误。
              if (caught === QrScanner.NO_QR_CODE_FOUND) {
                return;
              }
              setError("Unable to read QR code");
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

        setStatus("Scanning");
      } catch {
        if (!cancelled) {
          setError("Camera access failed");
          setStatus("Scanner unavailable");
        }
      }
    }

    void startScanner();

    return () => {
      cancelled = true;
      destroyScanner(scanner);
    };
  }, [onDetected]);

  return (
    <div className="modal-backdrop" role="presentation">
      <section className="qr-scanner-dialog" role="dialog" aria-modal="true" aria-label="Scan pairing QR">
        <header className="qr-scanner-header">
          <div className="qr-scanner-title">
            <Camera size={16} aria-hidden="true" />
            <span>Scan pairing QR</span>
          </div>
          <button type="button" className="icon-button" aria-label="Close scanner" onClick={onClose}>
            <X size={16} aria-hidden="true" />
          </button>
        </header>
        <div className="qr-scanner-video-wrap">
          <video ref={videoRef} className="qr-scanner-video" muted playsInline />
          <div className="qr-scanner-frame" aria-hidden="true" />
        </div>
        <div className={error ? "qr-scanner-status error" : "qr-scanner-status"}>{error ?? status}</div>
      </section>
    </div>
  );
}
