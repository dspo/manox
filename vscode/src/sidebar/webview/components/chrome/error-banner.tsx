import { CopyOnHover } from '../transcript/copy-on-hover';
import { Alert, AlertDescription } from '../ui/alert';

export type ErrorBannerProps = {
  message: string | null;
};

export const ErrorBanner = ({ message }: ErrorBannerProps) => {
  if (!message) {
    return null;
  }
  return (
    <Alert
      className="group font-chrome relative rounded-none border-x-0 pr-8 text-xs"
      variant="destructive"
    >
      <AlertDescription className="text-xs">{message}</AlertDescription>
      <CopyOnHover className="absolute top-1 right-1" text={message} />
    </Alert>
  );
};
