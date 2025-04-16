import type { Metadata } from "next";
import { Geist, Geist_Mono } from "next/font/google";
import "./globals.css";
import { ThemeProvider } from "@/components/theme-provider";
import { LoadingWrapper } from "@/components/loading-wrapper";
import { Toaster } from "@/components/ui/sonner";
import { ServerProvider } from "@/lib/server-context";
import { SidebarProvider } from "@/components/ui/sidebar";
import { SidebarWrapper } from "@/components/sidebar-wrapper";

const geistSans = Geist({
  variable: "--font-geist-sans",
  subsets: ["latin"],
});

const geistMono = Geist_Mono({
  variable: "--font-geist-mono",
  subsets: ["latin"],
});

export const metadata: Metadata = {
  title: "Agent-proxy Dashboard",
  description: "Agent-proxy Dashboard",
  icons: {
    icon: "/favicon.svg",
  },
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en" className="h-full" suppressHydrationWarning>
      <body
        className={`${geistSans.variable} ${geistMono.variable} antialiased h-full flex flex-col`}
      >
        <ServerProvider>
          <ThemeProvider
            attribute="class"
            defaultTheme="system"
            enableSystem
            disableTransitionOnChange
            storageKey="agentproxy-theme"
          >
            <LoadingWrapper>
              <SidebarProvider>
                <div className="flex min-h-screen w-full">
                  <SidebarWrapper />
                  <main className="flex-1 overflow-auto">
                    {children}
                  </main>
                </div>
              </SidebarProvider>
              <Toaster />
            </LoadingWrapper>
          </ThemeProvider>
        </ServerProvider>
      </body>
    </html>
  );
}
