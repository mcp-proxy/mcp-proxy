import { useState, useEffect } from "react";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Input } from "@/components/ui/input";
import { Checkbox } from "@/components/ui/checkbox";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { ChevronUp, ChevronDown } from "lucide-react";
import { Target, Header } from "@/lib/types";

interface SSETargetFormProps {
  targetName: string;
  onSubmit: (target: Target) => Promise<void>;
  isLoading: boolean;
  existingTarget?: Target;
  hideSubmitButton?: boolean;
}

export function SSETargetForm({
  targetName,
  onSubmit,
  isLoading,
  existingTarget,
  hideSubmitButton = false,
}: SSETargetFormProps) {
  const [sseUrl, setSseUrl] = useState("");
  const [showAdvancedSettings, setShowAdvancedSettings] = useState(false);
  const [headers, setHeaders] = useState<Header[]>([]);
  const [headerKey, setHeaderKey] = useState("");
  const [headerValue, setHeaderValue] = useState("");
  const [passthroughAuth, setPassthroughAuth] = useState(false);
  const [insecureSkipVerify, setInsecureSkipVerify] = useState(false);

  // Initialize form with existing target data if provided
  useEffect(() => {
    if (existingTarget?.sse) {
      const sse = existingTarget.sse;
      const protocol = sse.tls?.insecure_skip_verify ? "https" : "http";
      const url = `${protocol}://${sse.host}:${sse.port}${sse.path}`;
      setSseUrl(url);

      if (sse.headers) {
        setHeaders(sse.headers);
      }

      if (sse.auth?.passthrough) {
        setPassthroughAuth(true);
      }

      if (sse.tls?.insecure_skip_verify) {
        setInsecureSkipVerify(true);
      }
    }
  }, [existingTarget]);

  const addHeader = () => {
    if (headerKey && headerValue) {
      setHeaders([...headers, { key: headerKey, value: { string_value: headerValue } }]);
      setHeaderKey("");
      setHeaderValue("");
    }
  };

  const removeHeader = (index: number) => {
    setHeaders(headers.filter((_, i) => i !== index));
  };

  const handleSubmit = async () => {
    try {
      const urlObj = new URL(sseUrl);
      let port: number;
      if (urlObj.port) {
        port = parseInt(urlObj.port, 10);
      } else {
        port = urlObj.protocol === "https:" ? 443 : 80;
      }

      const target: Target = {
        name: targetName,
        sse: {
          host: urlObj.hostname,
          port: port,
          path: urlObj.pathname + urlObj.search,
          headers: headers.length > 0 ? headers : undefined,
        },
      };

      // Add auth if passthrough is enabled
      if (passthroughAuth) {
        target.sse!.auth = {
          passthrough: true,
        };
      }

      // Add TLS config if insecure skip verify is enabled
      if (insecureSkipVerify) {
        target.sse!.tls = {
          insecure_skip_verify: true,
        };
      }

      await onSubmit(target);
    } catch (err) {
      console.error("Error creating SSE target:", err);
      throw err;
    }
  };

  return (
    <form
      id="mcp-target-form"
      onSubmit={(e) => {
        e.preventDefault();
        handleSubmit();
      }}
      className="space-y-4 pt-4"
    >
      <div className="space-y-2">
        <Label htmlFor="sseUrl">Server URL</Label>
        <Input
          id="sseUrl"
          type="url"
          value={sseUrl}
          onChange={(e) => setSseUrl(e.target.value)}
          placeholder="http://localhost:3000/events"
          required
        />
        <p className="text-sm text-muted-foreground">
          Enter the full URL including protocol, hostname, port, and path
        </p>
      </div>

      <Collapsible open={showAdvancedSettings} onOpenChange={setShowAdvancedSettings}>
        <CollapsibleTrigger asChild>
          <Button variant="ghost" className="flex items-center p-0 h-auto">
            {showAdvancedSettings ? (
              <ChevronUp className="h-4 w-4 mr-1" />
            ) : (
              <ChevronDown className="h-4 w-4 mr-1" />
            )}
            Advanced Settings
          </Button>
        </CollapsibleTrigger>
        <CollapsibleContent className="space-y-4 pt-2">
          <div className="space-y-4">
            <div className="space-y-2">
              <Label>Headers</Label>
              <div className="space-y-2">
                {headers.map((header, index) => (
                  <div key={index} className="flex items-center gap-2">
                    <div className="flex-1">
                      <Input value={header.key} disabled placeholder="Header key" />
                    </div>
                    <div className="flex-1">
                      <Input
                        value={header.value.string_value}
                        disabled
                        placeholder="Header value"
                      />
                    </div>
                    <Button
                      type="button"
                      variant="outline"
                      size="icon"
                      onClick={() => removeHeader(index)}
                    >
                      <span className="sr-only">Remove header</span>
                      <svg
                        xmlns="http://www.w3.org/2000/svg"
                        width="24"
                        height="24"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        strokeWidth="2"
                        strokeLinecap="round"
                        strokeLinejoin="round"
                        className="h-4 w-4"
                      >
                        <path d="M18 6 6 18" />
                        <path d="m6 6 12 12" />
                      </svg>
                    </Button>
                  </div>
                ))}
                <div className="flex items-center gap-2">
                  <div className="flex-1">
                    <Input
                      value={headerKey}
                      onChange={(e) => setHeaderKey(e.target.value)}
                      placeholder="Header key"
                    />
                  </div>
                  <div className="flex-1">
                    <Input
                      value={headerValue}
                      onChange={(e) => setHeaderValue(e.target.value)}
                      placeholder="Header value"
                    />
                  </div>
                  <Button
                    type="button"
                    variant="outline"
                    onClick={addHeader}
                    disabled={!headerKey || !headerValue}
                  >
                    Add
                  </Button>
                </div>
              </div>
            </div>

            <div className="space-y-2">
              <Label>Authentication</Label>
              <div className="flex items-center space-x-2">
                <Checkbox
                  id="passthrough-auth"
                  checked={passthroughAuth}
                  onCheckedChange={(checked: boolean | "indeterminate") =>
                    setPassthroughAuth(checked as boolean)
                  }
                />
                <Label htmlFor="passthrough-auth" className="text-sm font-normal">
                  Pass through authentication
                </Label>
              </div>
            </div>

            <div className="space-y-2">
              <Label>TLS Configuration</Label>
              <div className="flex items-center space-x-2">
                <Checkbox
                  id="insecure-skip-verify"
                  checked={insecureSkipVerify}
                  onCheckedChange={(checked: boolean | "indeterminate") =>
                    setInsecureSkipVerify(checked as boolean)
                  }
                />
                <Label htmlFor="insecure-skip-verify" className="text-sm font-normal">
                  Insecure skip verify
                </Label>
              </div>
            </div>
          </div>
        </CollapsibleContent>
      </Collapsible>

      {!hideSubmitButton && (
        <Button type="submit" className="w-full" disabled={isLoading || !sseUrl}>
          {isLoading
            ? existingTarget
              ? "Updating Target..."
              : "Adding Target..."
            : existingTarget
              ? "Update SSE Target"
              : "Add SSE Target"}
        </Button>
      )}
    </form>
  );
}
