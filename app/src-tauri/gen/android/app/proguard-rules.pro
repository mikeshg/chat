# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile

# MM4 下载插件：tauri 经反射实例化插件并调用 @Command 方法，@InvokeArg 参数类由 Jackson 填充
-keep class com.chat.app.DownloadPlugin { *; }
-keep class com.chat.app.DownloadArgs { *; }
-keepattributes RuntimeVisibleAnnotations